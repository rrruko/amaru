// Copyright 2025 PRAGMA
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::echo::Envelope;
use amaru_consensus::{
    consensus::{
        chain_selection::{ChainSelector, ChainSelectorBuilder},
        receive_header::handle_chain_sync,
        select_chain::SelectChain,
        store::ChainStore,
        store_header::StoreHeader,
        validate_header::ValidateHeader,
        ChainSyncEvent, DecodedChainSyncEvent, ValidateHeaderEvent,
    },
    peer::Peer,
};
use amaru_kernel::{
    network::NetworkName,
    protocol_parameters::GlobalParameters,
    to_cbor, Hash, Header,
    Point::{self, *},
};
use amaru_stores::rocksdb::consensus::RocksDBStore;
use bytes::Bytes;
use clap::Parser;
use gasket::framework::WorkerError;
use ledger::{populate_chain_store, FakeStakeDistribution};
use std::{path::PathBuf, sync::Arc};
use sync::{
    mk_message, read_peer_addresses_from_init, ChainSyncMessage, MessageReader, OutputWriter,
    StdinMessageReader,
};
use tokio::sync::Mutex;
use tracing::info;

mod bytes;
mod ledger;
mod simulate;
mod sync;

#[derive(Debug, Parser)]
#[clap(name = "Amaru Simulator")]
#[clap(bin_name = "amaru-sim")]
#[clap(author, version, about, long_about = None)]
pub struct Args {
    /// Path of JSON-formatted stake distribution file.
    #[arg(long, default_value = "./stake_distribution.json")]
    pub stake_distribution_file: PathBuf,

    /// Path of JSON-formatted consensus context file.
    #[arg(long, default_value = "./consensus_context.json")]
    pub consensus_context_file: PathBuf,

    /// Path of the chain on-disk storage.
    #[arg(long, default_value = "./chain.db")]
    pub chain_dir: PathBuf,

    /// Path to the directory containing blockchain data such as epoch nonces.
    #[arg(long, default_value = "./data")]
    pub data_dir: PathBuf,

    /// Starting point for the (simulated) chain.
    /// Default to genesis hash, eg. all-zero hash.
    #[arg(long, default_value_t = Hash::from([0; 32]))]
    pub start_header: Hash<32>,
}

pub async fn run(args: Args) {
    let input_reader = StdinMessageReader::new();
    let consensus = bootstrap(args, input_reader);

    consensus.await;
}

pub async fn bootstrap<T: MessageReader>(args: Args, mut input_reader: T) {
    // NOTE: the output writer is behind a mutex because otherwise it's problematic to borrow
    // it as mutable in the inner loop of run simulator
    let output_writer = Arc::new(Mutex::new(OutputWriter::new()));

    let global_parameters = GlobalParameters::default();
    let stake_distribution: FakeStakeDistribution =
        FakeStakeDistribution::from_file(&args.stake_distribution_file, &global_parameters)
            .unwrap();
    let era_history = NetworkName::Testnet(42).into();

    let mut chain_store = RocksDBStore::new(&args.chain_dir, era_history).unwrap_or_else(|e| {
        panic!(
            "unable to open chain store at {}: {:?}",
            args.chain_dir.display(),
            e
        )
    });

    populate_chain_store(
        &mut chain_store,
        &args.start_header,
        &args.consensus_context_file,
    )
    .unwrap();

    let peer_addresses = read_peer_addresses_from_init(&mut input_reader)
        .await
        .unwrap();

    info!("using upstream peer addresses: {:?}", peer_addresses);

    {
        let mut w = output_writer.lock().await;
        let msg = Envelope {
            src: "n1".to_string(),
            dest: "c0".to_string(),
            body: ChainSyncMessage::InitOk { in_reply_to: 0 },
        };

        w.write(vec![msg]).await;
    }
    let chain_selector = make_chain_selector(
        Origin,
        &chain_store,
        &peer_addresses
            .iter()
            .map(|a| Peer::new(&a.clone()))
            .collect::<Vec<_>>(),
    );
    let chain_ref = Arc::new(Mutex::new(chain_store));
    let mut consensus = ValidateHeader::new(Box::new(stake_distribution), chain_ref.clone());

    let mut store_header = StoreHeader::new(chain_ref.clone());
    let mut select_chain = SelectChain::new(chain_selector);

    run_simulator(
        &mut input_reader,
        output_writer,
        chain_ref,
        &mut consensus,
        &mut store_header,
        &mut select_chain,
    )
    .await;
}

async fn run_simulator(
    input_reader: &mut impl MessageReader,
    output_writer: Arc<Mutex<OutputWriter>>,
    store: Arc<Mutex<dyn ChainStore<Header>>>,
    validate_header: &mut ValidateHeader,
    store_header: &mut StoreHeader,
    select_chain: &mut SelectChain,
) {
    loop {
        let span = tracing::info_span!("simulator");
        match input_reader.read().await {
            Err(err) => {
                tracing::error!("Error reading message: {:?}", err);
                break;
            }
            Ok(msg) => {
                // receive stage
                let chain_sync_event =
                    mk_message(msg, span).and_then(|chain_sync: ChainSyncEvent| {
                        handle_chain_sync(chain_sync).map_err(|_| WorkerError::Recv)
                    });

                // validate stage
                let validation_event = match chain_sync_event {
                    Ok(event) => match event {
                        DecodedChainSyncEvent::RollForward {
                            peer,
                            point,
                            header,
                            ..
                        } => validate_header
                            .handle_roll_forward(peer, point, header, &GlobalParameters::default())
                            .await
                            .expect("unexpected error on roll forward"),
                        DecodedChainSyncEvent::Rollback { .. } => event,
                    },
                    Err(_) => panic!("got error validating chain sync"),
                };

                // store header stage
                let store_event = match store_header.handle_event(validation_event).await {
                    Ok(stored) => stored,
                    Err(_) => panic!("got error storing event"),
                };

                // chain selection stage
                match select_chain.handle_chain_sync(store_event).await {
                    Ok(events) => {
                        let mut w = output_writer.lock().await;
                        write_events(&mut w, &store, &events).await;
                    }
                    Err(e) => {
                        tracing::error!("Error processing event: {:?}", e);
                        return;
                    }
                }
            }
        }
    }
    info!("no more messages to process, exiting");
}

async fn write_events(
    output_writer: &mut OutputWriter,
    store: &Arc<Mutex<dyn ChainStore<Header>>>,
    events: &[ValidateHeaderEvent],
) {
    let mut msgs = vec![];
    let s = store.lock().await;
    for e in events {
        match e {
            ValidateHeaderEvent::Validated { point, .. } => {
                let h: Hash<32> = point.into();
                let hdr = s.load_header(&h).unwrap();
                let fwd = ChainSyncMessage::Fwd {
                    msg_id: 0, // FIXME
                    slot: point.slot_or_default(),
                    hash: Bytes {
                        bytes: (*h).to_vec(),
                    },
                    header: Bytes {
                        bytes: to_cbor(&hdr),
                    },
                };
                let envelope = Envelope {
                    src: "n1".to_string(),
                    dest: "c1".to_string(),
                    body: fwd,
                };
                msgs.push(envelope);
            }
            ValidateHeaderEvent::Rollback { rollback_point, .. } => {
                let h: Hash<32> = rollback_point.into();
                let fwd = ChainSyncMessage::Bck {
                    msg_id: 0, // FIXME
                    slot: rollback_point.slot_or_default(),
                    hash: Bytes {
                        bytes: (*h).to_vec(),
                    },
                };
                let envelope = Envelope {
                    src: "n1".to_string(),
                    dest: "c1".to_string(),
                    body: fwd,
                };
                msgs.push(envelope);
            }
        }
    }

    output_writer.write(msgs).await;
}

fn make_chain_selector(
    tip: Point,
    chain_store: &impl ChainStore<Header>,
    peers: &Vec<Peer>,
) -> Arc<Mutex<ChainSelector<Header>>> {
    let mut builder = ChainSelectorBuilder::new();

    load_tip_from_store(chain_store, tip, &mut builder);

    for peer in peers {
        builder.add_peer(peer);
    }

    match builder.build() {
        Ok(chain_selector) => Arc::new(Mutex::new(chain_selector)),
        Err(e) => panic!("unable to build chain selector: {:?}", e),
    }
}

fn load_tip_from_store<'a>(
    chain_store: &impl ChainStore<Header>,
    tip: Point,
    builder: &'a mut ChainSelectorBuilder<Header>,
) -> &'a mut ChainSelectorBuilder<Header> {
    match tip {
        Origin => builder,
        Specific(..) => match chain_store.load_header(&From::from(&tip)) {
            None => panic!("Tip {:?} not found in chain store", tip),
            Some(header) => builder.set_tip(&header),
        },
    }
}
