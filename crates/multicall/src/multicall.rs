//! Multicall

use std::{sync::Arc, time::Duration};

use alloy_contract::{CallBuilder, RawCallBuilder};
use alloy_network::{Network, TransactionBuilder};
use alloy_primitives::{Address, Bytes};
use alloy_provider::Provider;
use alloy_sol_types::sol;
use alloy_transport::{Transport, TransportErrorKind, TransportResult};
use parking_lot::RwLock;
use tokio::{
    sync::mpsc::{self, UnboundedReceiver},
    task::JoinHandle,
    time,
};
sol! {
    #[sol(rpc)]
    #[derive(Debug)]
    contract Multicall3 {
        struct Call {
            address target;
            bytes callData;
        }

        /// @notice Backwards-compatible call aggregation with Multicall
        /// @param calls An array of Call structs
        /// @return blockNumber The block number where the calls were executed
        /// @return returnData An array of bytes containing the responses
        function aggregate(Call[] calldata calls) public payable returns (uint256 blockNumber, bytes[] memory returnData);
    }
}

use crate::Multicall3::{aggregateReturn as AggregateReturn, Call, Multicall3Instance};

/// Multicall
pub struct Multicall<T, P, N>
where
    P: Provider<T, N> + Clone + 'static,
    T: Transport + Clone,
    N: Network,
{
    /// Calls get drained every `interval` milliseconds. Default is 50ms.
    interval: Duration,
    /// Multicall3 Instance
    instance: Multicall3Instance<T, P, N>,
    /// Calls to be made
    calls: Arc<RwLock<Vec<RawCallBuilder<T, P, N>>>>,
}

impl<T, P, N> Multicall<T, P, N>
where
    P: Provider<T, N> + Clone + 'static,
    T: Transport + Clone,
    N: Network,
{
    /// Create a new Multicall instance
    pub fn new(address: Address, provider: P) -> Self {
        Self {
            interval: Duration::from_millis(50),
            instance: Multicall3::new(address, provider),
            calls: Default::default(),
        }
    }

    /// Set the interval (milliseconds) at which calls are drained
    pub fn with_interval(mut self, interval: u64) -> Self {
        self.interval = Duration::from_millis(interval);
        self
    }

    /// Add a call to the Multicall
    pub fn add_call<D>(&self, call: &CallBuilder<T, &P, D, N>) {
        let req = call.as_ref();

        let raw = RawCallBuilder::new_raw(
            call.provider,
            req.input().map_or(Bytes::new(), |input| input.clone()),
        )
        .to(req.to().unwrap_or_default())
        .with_cloned_provider();

        let mut calls = self.calls.write();

        calls.push(raw);
    }

    /// Execute the calls
    ///
    /// Note:
    ///
    /// The calls are executed in the order they are added.
    pub async fn call(self) -> TransportResult<AggregateReturn> {
        let builders = self.calls.write().drain(..).collect::<Vec<_>>();
        let mut calls = Vec::new();
        for call in builders {
            let tx = call.as_ref().clone();
            if tx.to().is_none() || tx.kind().is_some_and(|k| k.is_create()) {
                return Err(TransportErrorKind::custom_str("invalid `to` address"));
            }

            calls.push(Call {
                target: tx.to().unwrap(),
                callData: tx.input().map_or(Bytes::new(), |input| input.clone()),
            });
        }

        self.instance.aggregate(calls).call().await.map_err(TransportErrorKind::custom)
    }

    /// Spawn a task to execute the calls every `interval` milliseconds
    pub fn spawn_task(
        self,
    ) -> (JoinHandle<TransportResult<()>>, UnboundedReceiver<TransportResult<AggregateReturn>>)
    {
        let instance = self.instance.clone();
        let calls = self.calls.clone();
        let mut interval = time::interval(self.interval);
        let (tx, rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            loop {
                interval.tick().await;

                if tx.is_closed() {
                    break;
                }

                let builders = calls.write().drain(..).collect::<Vec<_>>();
                if builders.is_empty() {
                    continue;
                }

                let mut multicall_calls = Vec::new();
                for call in builders {
                    let tx = call.as_ref().clone();
                    if tx.to().is_none() || tx.kind().is_some_and(|k| k.is_create()) {
                        return Err(TransportErrorKind::custom_str("invalid `to` address"));
                    }

                    multicall_calls.push(Call {
                        target: tx.to().unwrap(),
                        callData: tx.input().map_or(Bytes::new(), |input| input.clone()),
                    });
                }

                let aggregate = instance.aggregate(multicall_calls);
                let result = match aggregate.call().await {
                    Ok(result) => Ok(result),
                    Err(e) => Err(TransportErrorKind::custom(e)),
                };

                if tx.send(result).is_err() {
                    // Receiver dropped, exit the loop
                    break;
                }
            }
            Ok(())
        });

        (handle, rx)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_node_bindings::Anvil;
    use alloy_primitives::address;
    use alloy_provider::ProviderBuilder;

    sol! {
        #[sol(rpc)]
        #[derive(Debug)]
        contract IERC20 {
            function totalSupply() external view returns (uint256);
            function name() external view returns (string memory);
            function symbol() external view returns (string memory);
            function decimals() external view returns (uint8);
        }
    }

    #[tokio::test]
    async fn test_mutlticall_call() {
        let fork_url = "https://eth-mainnet.alchemyapi.io/v2/jGiK5vwDfC3F4r0bqukm-W2GqgdrxdSr";
        let fork_block_number = 21112416;
        let anvil = Anvil::new().fork(fork_url).fork_block_number(fork_block_number).spawn();
        let multicall_address = address!("cA11bde05977b3631167028862bE2a173976CA11");
        let provider = ProviderBuilder::new().on_http(anvil.endpoint_url());

        let mut multicall = Multicall::new(multicall_address, provider.clone());

        let weth_addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let weth = IERC20::new(weth_addr, provider.clone());

        let total_supply = weth.totalSupply();
        let name = weth.name();
        let symbol = weth.symbol();
        let decimals = weth.decimals();

        multicall.add_call(&total_supply);
        multicall.add_call(&name);
        multicall.add_call(&symbol);
        multicall.add_call(&decimals);

        let result = multicall.call().await.unwrap();

        let block_number = result.blockNumber;

        assert_eq!(block_number.to::<u64>(), fork_block_number);
        let return_data = result.returnData;

        // ABI decode the return data
        for (i, return_data) in return_data.into_iter().enumerate() {
            match i {
                0 => {
                    let total_supply =
                        total_supply.decode_output(return_data.clone(), true).unwrap();
                    println!("Total Supply: {:?}", total_supply);
                }
                1 => {
                    let name = name.decode_output(return_data.clone(), true).unwrap();
                    println!("Name: {:?}", name);
                }
                2 => {
                    let symbol = symbol.decode_output(return_data.clone(), true).unwrap();
                    println!("Symbol: {:?}", symbol);
                }
                3 => {
                    let decimals = decimals.decode_output(return_data.clone(), true).unwrap();
                    println!("Decimals: {:?}", decimals);
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn multicall_task() {
        let fork_url = "https://eth-mainnet.alchemyapi.io/v2/jGiK5vwDfC3F4r0bqukm-W2GqgdrxdSr";
        let fork_block_number = 21112416;
        let anvil = Anvil::new().fork(fork_url).fork_block_number(fork_block_number).spawn();
        let multicall_address = address!("cA11bde05977b3631167028862bE2a173976CA11");
        let provider = ProviderBuilder::new().on_http(anvil.endpoint_url());

        let mut multicall = Multicall::new(multicall_address, provider.clone()).with_interval(1);

        let weth_addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let weth = IERC20::new(weth_addr, provider.clone());

        let total_supply = weth.totalSupply();
        let name = weth.name();
        let symbol = weth.symbol();
        let decimals = weth.decimals();

        multicall.add_call(&total_supply);
        multicall.add_call(&name);
        multicall.add_call(&symbol);
        multicall.add_call(&decimals);

        let (handle, mut rx) = multicall.spawn_task();

        let recv = rx.recv().await;

        match recv {
            Some(Ok(result)) => {
                let block_number = result.blockNumber;
                assert_eq!(block_number.to::<u64>(), fork_block_number);
                let return_data = result.returnData;

                // ABI decode the return data
                for (i, return_data) in return_data.into_iter().enumerate() {
                    match i {
                        0 => {
                            let total_supply =
                                total_supply.decode_output(return_data.clone(), true).unwrap();
                            println!("Total Supply: {:?}", total_supply);
                        }
                        1 => {
                            let name = name.decode_output(return_data.clone(), true).unwrap();
                            println!("Name: {:?}", name);
                        }
                        2 => {
                            let symbol = symbol.decode_output(return_data.clone(), true).unwrap();
                            println!("Symbol: {:?}", symbol);
                        }
                        3 => {
                            let decimals =
                                decimals.decode_output(return_data.clone(), true).unwrap();
                            println!("Decimals: {:?}", decimals);
                        }
                        _ => {}
                    }
                }
            }
            Some(Err(e)) => {
                println!("Error: {:?}", e);
            }
            None => {
                println!("Receiver dropped");
            }
        };

        drop(rx);
        let _ = handle.await.unwrap().unwrap();
    }
}
