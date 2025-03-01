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

use std::future::Future;

use risingwave_common::array::DataChunk;
use risingwave_common::error::ErrorCode::InternalError;
use risingwave_common::error::{Result, ToRwResult};
use risingwave_pb::batch_plan::exchange_info::BroadcastInfo;
use risingwave_pb::batch_plan::*;
use tokio::sync::mpsc;

use crate::task::channel::{ChanReceiver, ChanReceiverImpl, ChanSender, ChanSenderImpl};

/// `BroadcastSender` sends the same chunk to a number of `BroadcastReceiver`s.
pub struct BroadcastSender {
    senders: Vec<mpsc::UnboundedSender<Option<DataChunk>>>,
    broadcast_info: BroadcastInfo,
}

impl ChanSender for BroadcastSender {
    type SendFuture<'a> = impl Future<Output = Result<()>>;

    fn send(&mut self, chunk: Option<DataChunk>) -> Self::SendFuture<'_> {
        async move {
            self.senders.iter().try_for_each(|sender| {
                sender
                    .send(chunk.clone())
                    .to_rw_result_with(|| "BroadcastSender::send".into())
            })
        }
    }
}

/// One or more `BroadcastReceiver`s corresponds to a single `BroadcastReceiver`
pub struct BroadcastReceiver {
    receiver: mpsc::UnboundedReceiver<Option<DataChunk>>,
}

impl ChanReceiver for BroadcastReceiver {
    type RecvFuture<'a> = impl Future<Output = Result<Option<DataChunk>>>;

    fn recv(&mut self) -> Self::RecvFuture<'_> {
        async move {
            match self.receiver.recv().await {
                Some(data_chunk) => Ok(data_chunk),
                // Early close should be treated as an error.
                None => Err(InternalError("broken broadcast_channel".to_string()).into()),
            }
        }
    }
}

pub fn new_broadcast_channel(shuffle: &ExchangeInfo) -> (ChanSenderImpl, Vec<ChanReceiverImpl>) {
    let broadcast_info = match shuffle.distribution {
        Some(exchange_info::Distribution::BroadcastInfo(ref v)) => v.clone(),
        _ => exchange_info::BroadcastInfo::default(),
    };

    let output_count = broadcast_info.count as usize;
    let mut senders = Vec::with_capacity(output_count);
    let mut receivers = Vec::with_capacity(output_count);
    for _ in 0..output_count {
        let (s, r) = mpsc::unbounded_channel();
        senders.push(s);
        receivers.push(r);
    }
    let channel_sender = ChanSenderImpl::Broadcast(BroadcastSender {
        senders,
        broadcast_info,
    });
    let channel_receivers = receivers
        .into_iter()
        .map(|receiver| ChanReceiverImpl::Broadcast(BroadcastReceiver { receiver }))
        .collect::<Vec<_>>();
    (channel_sender, channel_receivers)
}
