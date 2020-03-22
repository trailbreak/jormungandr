use super::grpc;
use crate::blockcfg::{Block, HeaderDesc, HeaderHash};
use crate::blockchain::{self, Blockchain, Error as BlockchainError, PreCheckedHeader, Ref, Tip};
use crate::settings::start::network::Peer;
use chain_core::property::HasHeader;
use futures03::{
    channel::oneshot::Receiver,
    compat::*,
    future::{Either, Shared},
    prelude::*,
    stream,
    task::Poll,
};
use network_core::client::{BlockService, Client as _, GossipService};
use network_core::error::Error as NetworkError;
use slog::Logger;
use thiserror::Error;
use tokio_compat::runtime::TaskExecutor;

use std::fmt::Debug;
use std::pin::Pin;
use std::sync::Arc;

#[derive(Error, Debug)]
pub enum Error {
    #[error("failed to connect to bootstrap peer")]
    Connect(#[from] grpc::ConnectError),
    #[error("connection broken")]
    ClientNotReady(#[source] NetworkError),
    #[error("peers not available")]
    PeersNotAvailable(#[source] NetworkError),
    #[error("bootstrap pull request failed")]
    PullRequestFailed(#[source] NetworkError),
    #[error("bootstrap pull stream failed")]
    PullStreamFailed(#[source] NetworkError),
    #[error("block header check failed")]
    HeaderCheckFailed(#[source] BlockchainError),
    #[error(
        "received block {0} is already present, but does not descend from any of the checkpoints"
    )]
    BlockNotOnBranch(HeaderHash),
    #[error("received block {0} is not connected to the block chain")]
    BlockMissingParent(HeaderHash),
    #[error("failed to fetch checkpoints from storage")]
    GetCheckpointsFailed(#[source] BlockchainError),
    #[error("failed to apply block to the blockchain")]
    ApplyBlockFailed(#[source] BlockchainError),
    #[error("failed to select the new tip")]
    ChainSelectionFailed(#[source] BlockchainError),
    #[error("the bootstrap process was interrupted")]
    Interrupted,
}

pub async fn peers_from_trusted_peer(
    peer: &Peer,
    logger: Logger,
    executor: TaskExecutor,
) -> Result<Vec<Peer>, Error> {
    info!(
        logger,
        "getting peers from bootstrap peer {}", peer.connection
    );

    let mut client = grpc::connect(&peer, None, executor)
        .compat()
        .await?
        .ready()
        .compat()
        .await
        .map_err(|e| Error::ClientNotReady(e))?;

    let peers = client
        .peers()
        .compat()
        .await
        .map_err(|e| Error::PeersNotAvailable(e))?;

    info!(
        logger,
        "peer {} : peers known : {}",
        peer.connection,
        peers.peers.len()
    );

    Ok(peers
        .peers
        .iter()
        .map(|peer| Peer::new(peer.addr))
        .collect())
}

pub async fn bootstrap_from_peer(
    peer: &Peer,
    blockchain: Blockchain,
    tip: Tip,
    bootstrap_stopper: Shared<Receiver<()>>,
    logger: Logger,
    executor: TaskExecutor,
) -> Result<(), Error> {
    use futures03::future::{select, try_join};

    debug!(logger, "connecting to bootstrap peer {}", peer.connection);

    let blockchain1 = blockchain.clone();
    let tip1 = tip.clone();

    let stream_future = async move {
        let client = grpc::connect(&peer, None, executor).compat().await?;

        let (mut client, checkpoints) = try_join(
            client
                .ready()
                .compat()
                .map_err(|e| Error::ClientNotReady(e)),
            blockchain1
                .get_checkpoints(tip1.branch())
                .map(|res| Ok(res)),
        )
        .await?;

        client
            .pull_blocks_to_tip(checkpoints.as_slice())
            .compat()
            .map_ok(|stream| stream.compat())
            .map_err(|e| Error::PullRequestFailed(e))
            .await
    };

    // process a signal from the stopper if it arrives before the stream is ready
    match select(stream_future.boxed(), bootstrap_stopper).await {
        Either::Left((stream_result, bootstrap_stopper)) => {
            bootstrap_from_stream(blockchain, tip, stream_result?, bootstrap_stopper, logger).await
        }
        Either::Right((bootstrap_stopper_result, _)) => match bootstrap_stopper_result {
            Ok(()) => Err(Error::Interrupted),
            Err(_) => panic!("failed to wait for SIGINT"),
        },
    }
}

struct BootstrapInfo {
    last_reported: std::time::SystemTime,
    last_bytes_received: u64,
    bytes_received: u64,
    block_received: u64,
    last_block_description: Option<HeaderDesc>,
}

impl BootstrapInfo {
    pub fn new() -> Self {
        let now = std::time::SystemTime::now();
        let lbd: Option<HeaderDesc> = None;
        BootstrapInfo {
            last_reported: now,
            last_bytes_received: 0,
            bytes_received: 0,
            block_received: 0,
            last_block_description: lbd,
        }
    }

    pub fn append_block(&mut self, b: &Block) {
        use chain_core::property::Serialize;
        self.bytes_received += b.serialize_as_vec().unwrap().len() as u64; // TODO sad serialization back
        self.block_received += 1;
        self.last_block_description = Some(b.header.description());
    }

    pub fn report(&mut self, logger: &Logger) {
        fn print_sz(n: f64) -> String {
            if n > 1_000_000.0 {
                format!("{:.2}mb", n / (1024 * 1024) as f64)
            } else if n > 1_000.0 {
                format!("{:.2}kb", n / 1024 as f64)
            } else {
                format!("{:.2}b", n)
            }
        }
        let current = std::time::SystemTime::now();
        let time_diff = current.duration_since(self.last_reported);
        let bytes_diff = self.bytes_received - self.last_bytes_received;

        let bytes = print_sz(bytes_diff as f64);
        let kbs = time_diff
            .map(|td| {
                let v = (bytes_diff as f64) / td.as_secs_f64();
                print_sz(v)
            })
            .unwrap_or("N/A".to_string());

        self.last_reported = current;
        self.last_bytes_received = self.bytes_received;
        info!(
            logger,
            "receiving from network bytes={} {}/s, blockchain {}",
            bytes,
            kbs,
            self.last_block_description
                .as_ref()
                .map(|lbd| lbd.to_string())
                .expect("append_block should always be called before report")
        )
    }
}

async fn bootstrap_from_stream<S>(
    mut blockchain: Blockchain,
    branch: Tip,
    stream: S,
    bootstrap_stopper: Shared<Receiver<()>>,
    logger: Logger,
) -> Result<(), Error>
where
    S: Stream<Item = Result<Block, NetworkError>> + Unpin,
{
    const PROCESS_LOGGING_DISTANCE: u64 = 2500;
    let block0 = blockchain.block0().clone();

    let mut bootstrap_info = BootstrapInfo::new();
    let mut maybe_parent_tip = None;

    let mut stream = stream.map_err(Error::PullStreamFailed);
    let mut bootstrap_stopper = bootstrap_stopper.map(|res| match res {
        Ok(()) => Err(Error::Interrupted),
        Err(_) => panic!("failed to wait for SIGINT"),
    });

    // This stream will either end when the block stream is exhausted or when
    // the cancellation signal arrives. Building such stream allows us to
    // correctly write all blocks and update the block tip upon the arrival of
    // the cancellation signal.
    let mut stream = stream::poll_fn(move |cx| {
        let bootstrap_stopper = Pin::new(&mut bootstrap_stopper);
        match bootstrap_stopper.poll(cx) {
            Poll::Pending => {
                let stream = Pin::new(&mut stream);
                stream.poll_next(cx)
            }
            Poll::Ready(value) => Poll::Ready(Some(value)),
        }
    });

    while let Some(block_result) = stream.next().await {
        let result = match block_result {
            Ok(block) => {
                if block.header.hash() == block0 {
                    continue;
                }

                bootstrap_info.append_block(&block);

                if bootstrap_info.block_received % PROCESS_LOGGING_DISTANCE == 0 {
                    bootstrap_info.report(&logger);
                }

                handle_block(&blockchain, block, &logger).await
            }
            Err(err) => Err(err),
        };

        match result {
            Ok(parent_tip) => {
                maybe_parent_tip = Some(parent_tip);
            }
            Err(err) => {
                if let Some(parent_tip) = maybe_parent_tip {
                    if let Err(err) = blockchain::process_new_ref(
                        &logger,
                        &mut blockchain,
                        branch.clone(),
                        parent_tip.clone(),
                    )
                    .await
                    {
                        warn!(logger, "couldn't gracefully exit from failed netboot"; "error" => ?err);
                    }
                }
                return Err(err);
            }
        }
    }

    if let Some(parent_tip) = maybe_parent_tip {
        blockchain::process_new_ref(&logger, &mut blockchain, branch, parent_tip)
            .await
            .map_err(|e| Error::ChainSelectionFailed(e))
    } else {
        info!(logger, "no new blocks in bootstrap stream");
        Ok(())
    }
}

async fn handle_block(
    blockchain: &Blockchain,
    block: Block,
    logger: &Logger,
) -> Result<Arc<Ref>, Error> {
    let header = block.header();
    let pre_checked = blockchain
        .pre_check_header(header, true)
        .await
        .map_err(|e| Error::HeaderCheckFailed(e))?;
    match pre_checked {
        PreCheckedHeader::AlreadyPresent {
            cached_reference: Some(block_ref),
            ..
        } => Ok(block_ref),
        PreCheckedHeader::AlreadyPresent {
            cached_reference: None,
            header,
        } => Err(Error::BlockNotOnBranch(header.hash())),
        PreCheckedHeader::MissingParent { header, .. } => {
            Err(Error::BlockMissingParent(header.hash()))
        }
        PreCheckedHeader::HeaderWithCache { header, parent_ref } => {
            let post_checked = blockchain
                .post_check_header(header, parent_ref, blockchain::CheckHeaderProof::Enabled)
                .await
                .map_err(|e| Error::HeaderCheckFailed(e))?;

            debug!(
                logger,
                "validated block";
                "hash" => %post_checked.header().hash(),
                "block_date" => %post_checked.header().block_date(),
            );
            let applied = blockchain
                .apply_and_store_block(post_checked, block)
                .await
                .map_err(|e| Error::ApplyBlockFailed(e))?;
            Ok(applied.cached_ref())
        }
    }
}
