// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::num::NonZeroUsize;

use futures_async_stream::try_stream;
use num_traits::CheckedSub;
use risingwave_common::array::column::Column;
use risingwave_common::array::DataChunk;
use risingwave_common::catalog::Schema;
use risingwave_common::error::{ErrorCode, Result, RwError};
use risingwave_common::types::{DataType, IntervalUnit, ScalarImpl};
use risingwave_expr::expr::expr_binary_nonnull::new_binary_expr;
use risingwave_expr::expr::{Expression, InputRefExpression, LiteralExpression};
use risingwave_pb::batch_plan::plan_node::NodeBody;
use risingwave_pb::expr::expr_node;

use crate::executor::ExecutorBuilder;
use crate::executor2::{BoxedDataChunkStream, BoxedExecutor2, BoxedExecutor2Builder, Executor2};
pub struct HopWindowExecutor2 {
    child: BoxedExecutor2,
    schema: Schema,
    identity: String,
    pub time_col_idx: usize,
    pub window_slide: IntervalUnit,
    pub window_size: IntervalUnit,
}

impl HopWindowExecutor2 {
    pub fn new(
        child: BoxedExecutor2,
        schema: Schema,
        identity: String,
        time_col_idx: usize,
        window_slide: IntervalUnit,
        window_size: IntervalUnit,
    ) -> Self {
        Self {
            child,
            schema,
            identity,
            time_col_idx,
            window_slide,
            window_size,
        }
    }
}

impl Executor2 for HopWindowExecutor2 {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    fn execute(self: Box<Self>) -> BoxedDataChunkStream {
        self.do_execute()
    }
}

impl HopWindowExecutor2 {
    #[try_stream(boxed, ok = DataChunk, error = RwError)]
    async fn do_execute(self: Box<Self>) {
        let time_col_idx = self.time_col_idx;
        let window_slide = self.window_slide;
        let window_size = self.window_size;

        let units = window_size
            .exact_div(&window_slide)
            .and_then(|x| NonZeroUsize::new(usize::try_from(x).ok()?))
            .ok_or_else(|| {
                RwError::from(ErrorCode::UnknownError(format!(
                    "window_size {} cannot be divided by window_slide {}",
                    window_size, window_slide
                )))
            })?
            .get();

        let schema = self.schema();
        let time_col_data_type = schema.fields()[time_col_idx].data_type();
        let time_col_ref = InputRefExpression::new(time_col_data_type, self.time_col_idx).boxed();

        let window_slide_expr =
            LiteralExpression::new(DataType::Interval, Some(ScalarImpl::Interval(window_slide)))
                .boxed();

        // The first window_start of hop window should be:
        // tumble_start(`time_col` - (`window_size` - `window_slide`), `window_slide`).
        // Let's pre calculate (`window_size` - `window_slide`).
        let window_size_sub_slide = window_size.checked_sub(&window_slide).ok_or_else(|| {
            RwError::from(ErrorCode::UnknownError(format!(
                "window_size {} cannot be subtracted by window_slide {}",
                window_size, window_slide
            )))
        })?;
        let window_size_sub_slide_expr = LiteralExpression::new(
            DataType::Interval,
            Some(ScalarImpl::Interval(window_size_sub_slide)),
        )
        .boxed();

        let hop_start = new_binary_expr(
            expr_node::Type::TumbleStart,
            risingwave_common::types::DataType::Timestamp,
            new_binary_expr(
                expr_node::Type::Subtract,
                DataType::Timestamp,
                time_col_ref,
                window_size_sub_slide_expr,
            ),
            window_slide_expr,
        );

        #[for_await]
        for data_chunk in self.child.execute() {
            let data_chunk = data_chunk?;
            let data_chunk = data_chunk.compact()?;

            let hop_start = hop_start.eval(&data_chunk)?;
            let hop_start_chunk = DataChunk::new(vec![Column::new(hop_start)], None);
            let (origin_cols, visibility) = data_chunk.into_parts();
            // SAFETY: Already compacted.
            assert!(visibility.is_none());
            for i in 0..units {
                let window_start_offset = window_slide.checked_mul_int(i).ok_or_else(|| {
                    RwError::from(ErrorCode::UnknownError(format!(
                        "window_slide {} cannot be multiplied by {}",
                        window_slide, i
                    )))
                })?;
                let window_start_offset_expr = LiteralExpression::new(
                    DataType::Interval,
                    Some(ScalarImpl::Interval(window_start_offset)),
                )
                .boxed();
                let window_end_offset =
                    window_slide.checked_mul_int(i + units).ok_or_else(|| {
                        RwError::from(ErrorCode::UnknownError(format!(
                            "window_slide {} cannot be multiplied by {}",
                            window_slide, i
                        )))
                    })?;
                let window_end_offset_expr = LiteralExpression::new(
                    DataType::Interval,
                    Some(ScalarImpl::Interval(window_end_offset)),
                )
                .boxed();
                let window_start_expr = new_binary_expr(
                    expr_node::Type::Add,
                    DataType::Timestamp,
                    InputRefExpression::new(DataType::Timestamp, 0).boxed(),
                    window_start_offset_expr,
                );
                let window_start_col = window_start_expr.eval(&hop_start_chunk)?;
                let window_end_expr = new_binary_expr(
                    expr_node::Type::Add,
                    DataType::Timestamp,
                    InputRefExpression::new(DataType::Timestamp, 0).boxed(),
                    window_end_offset_expr,
                );
                let window_end_col = window_end_expr.eval(&hop_start_chunk)?;
                let mut new_cols = origin_cols.clone();
                new_cols.extend_from_slice(&[
                    Column::new(window_start_col),
                    Column::new(window_end_col),
                ]);
                let new_chunk = DataChunk::try_from(new_cols)?;
                yield new_chunk;
            }
        }
    }
}

impl BoxedExecutor2Builder for HopWindowExecutor2 {
    fn new_boxed_executor2(source: &ExecutorBuilder) -> Result<BoxedExecutor2> {
        let node = try_match_expand!(
            source.plan_node().get_node_body().unwrap(),
            NodeBody::HopWindow
        )?;

        let proto_child = source.plan_node.get_children().get(0).ok_or_else(|| {
            RwError::from(ErrorCode::InternalError(String::from(
                "Child interpreting error",
            )))
        })?;
        let child = source.clone_for_plan(proto_child).build2()?;

        let schema = child.schema().clone();
        let identity = source.plan_node().get_identity().clone();
        let time_col_idx = node.get_time_col()?.column_idx as usize;
        let window_slide = node.get_window_slide()?.into();
        let window_size = node.get_window_size()?.into();

        Ok(Box::new(HopWindowExecutor2::new(
            child,
            schema,
            identity,
            time_col_idx,
            window_slide,
            window_size,
        )))
    }
}

#[cfg(test)]
mod tests {
    use futures::stream::StreamExt;
    use risingwave_common::catalog::{Field, Schema};
    use risingwave_common::test_prelude::*;
    use risingwave_common::types::DataType;

    use super::*;
    use crate::executor::test_utils::MockExecutor;
    use crate::executor2::Executor2;
    use crate::*;

    #[tokio::test]
    async fn test_project_executor() -> Result<()> {
        let field1 = Field::unnamed(DataType::Int64);
        let field2 = Field::unnamed(DataType::Int64);
        let field3 = Field::with_name(DataType::Timestamp, "created_at");
        let schema = Schema::new(vec![field1, field2, field3]);

        let chunk = DataChunk::from_pretty(
            &"I I TS
              1 1 ^10:00:00
              2 3 ^10:05:00
              3 2 ^10:14:00
              4 1 ^10:22:00
              5 3 ^10:33:00
              6 2 ^10:42:00
              7 1 ^10:51:00
              8 3 ^11:02:00"
                .replace('^', "2022-2-2T"),
        );

        let mut input = Box::new(MockExecutor::new(schema.clone()));
        input.add(chunk);

        let window_slide = IntervalUnit::from_minutes(15);
        let window_size = IntervalUnit::from_minutes(30);

        let executor = Box::new(HopWindowExecutor2::new(
            input,
            schema,
            "HopWindowExecutor2".to_string(),
            2,
            window_slide,
            window_size,
        ));

        let mut stream = executor.execute();
        let chunk = stream.next().await.unwrap().unwrap();

        assert_eq!(
            chunk,
            DataChunk::from_pretty(
                &"I I TS        TS        TS
                  1 1 ^10:00:00 ^09:45:00 ^10:15:00
                  2 3 ^10:05:00 ^09:45:00 ^10:15:00
                  3 2 ^10:14:00 ^09:45:00 ^10:15:00
                  4 1 ^10:22:00 ^10:00:00 ^10:30:00
                  5 3 ^10:33:00 ^10:15:00 ^10:45:00
                  6 2 ^10:42:00 ^10:15:00 ^10:45:00
                  7 1 ^10:51:00 ^10:30:00 ^11:00:00
                  8 3 ^11:02:00 ^10:45:00 ^11:15:00"
                    .replace('^', "2022-2-2T"),
            )
        );

        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(
            chunk,
            DataChunk::from_pretty(
                &"I I TS        TS        TS
                  1 1 ^10:00:00 ^10:00:00 ^10:30:00
                  2 3 ^10:05:00 ^10:00:00 ^10:30:00
                  3 2 ^10:14:00 ^10:00:00 ^10:30:00
                  4 1 ^10:22:00 ^10:15:00 ^10:45:00
                  5 3 ^10:33:00 ^10:30:00 ^11:00:00
                  6 2 ^10:42:00 ^10:30:00 ^11:00:00
                  7 1 ^10:51:00 ^10:45:00 ^11:15:00
                  8 3 ^11:02:00 ^11:00:00 ^11:30:00"
                    .replace('^', "2022-2-2T"),
            )
        );
        Ok(())
    }
}
