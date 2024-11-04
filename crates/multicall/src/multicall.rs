//! Multicall

use std::time::Duration;

use alloy_contract::{CallBuilder, CallDecoder, RawCallBuilder};
use alloy_network::{Network, TransactionBuilder};
use alloy_primitives::{Address, Bytes};
use alloy_provider::Provider;
use alloy_sol_types::sol;
use alloy_transport::{Transport, TransportErrorKind, TransportResult};

sol! {
    #[sol(rpc)]
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
    /// Address of the deployed Multicall3 contract
    address: Address,
    /// Calls get drained every `interval` milliseconds. Default is 50ms.
    interval: Duration,
    /// Provider
    provider: P,
    /// Multicall3 Instance
    instance: Multicall3Instance<T, P, N>,
    /// Calls to be made
    calls: Vec<RawCallBuilder<T, P, N>>,
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
            address,
            interval: Duration::from_millis(50),
            instance: Multicall3::new(address, provider.clone()),
            provider,
            calls: Default::default(),
        }
    }

    /// Set the interval at which calls are drained
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Add a call to the Multicall
    pub fn add_call(&mut self, call: RawCallBuilder<T, &P, N>) {
        self.calls.push(call.with_cloned_provider());
    }

    /// Add multiple calls
    pub fn add_calls(&mut self, calls: Vec<RawCallBuilder<T, &P, N>>) {
        for call in calls {
            self.add_call(call);
        }
    }

    /// Execute the calls
    pub async fn call(self) -> TransportResult<AggregateReturn> {
        let mut calls = Vec::new();
        for call in &self.calls {
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
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_node_bindings::Anvil;
    use alloy_primitives::address;
    use alloy_provider::ProviderBuilder;

    #[tokio::test]
    async fn test_mutlticall() {
        let fork_url = "https://eth-mainnet.alchemyapi.io/v2/jGiK5vwDfC3F4r0bqukm-W2GqgdrxdSr";
        let fork_block_number = 21112416;
        let anvil = Anvil::new().fork(fork_url).fork_block_number(fork_block_number).spawn();
        let multicall_address = address!("cA11bde05977b3631167028862bE2a173976CA11");
        let provider = ProviderBuilder::new().on_http(anvil.endpoint_url());

        let mut multicall = Multicall::new(multicall_address, provider.clone());

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

        let weth_addr = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let weth = IERC20::new(weth_addr, provider.clone());

        let total_supply = weth.totalSupply();
        let name = weth.name();
        let symbol = weth.symbol();
        let decimals = weth.decimals();

        let calls = vec![
            total_supply.clone().clear_decoder(),
            name.clone().clear_decoder(),
            symbol.clone().clear_decoder(),
            decimals.clone().clear_decoder(),
        ];

        multicall.add_calls(calls.clone());

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
}
