use std::{any::Any, collections::HashMap};
use std::str::FromStr;
use alloy::primitives::{Address as AlloyAddress, U256};
use num_bigint::{BigUint, ToBigUint};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{
        token::Token,
        Address
    },
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::evm::protocol::{
    cpmm::protocol::{
        cpmm_delta_transition, cpmm_fee, cpmm_get_amount_out, cpmm_get_limits, cpmm_spot_price,
    },
    safe_math::{safe_add_u256, safe_sub_u256, safe_div_u256, safe_mul_u256},
    utils::uniswap::{
            solidity_math::{sqrt_u256},
    },
    u256_num::{biguint_to_u256, u256_to_biguint},
};

const UNISWAP_V2_FEE_BPS: u32 = 30; // 0.3% fee

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniswapV2State {
    pub reserve0: U256,
    pub reserve1: U256,
    pub balance0: U256,
    pub balance1: U256,
    pub liquidity: U256,
    pub total_supply: U256,
    pub k_last: Option<U256>,
} 
    // total_supply_mut : &mut U256, since we are getting it via entrypoint then its not a delta transition effect

impl UniswapV2State {
    /// Creates a new instance of `UniswapV2State` with the given reserves.
    ///
    /// # Arguments
    ///
    /// * `reserve0` - Reserve of token 0.
    /// * `reserve1` - Reserve of token 1.
    /// * `balance0` - Balance of token 0 in the pair contract.
    /// * `balance1` - Balance of token 1 in the pair contract
    /// * `liquidity` - Balance of lp tokens in the pair contract.
    /// * `total_supply` - total circulating supply of lp_tokens.
    /// * `k_last` - last balance of 
    pub fn new(
            reserve0: U256, 
            reserve1: U256, 
            balance0: U256, 
            balance1: U256,
            liquidity: U256,
            total_supply: U256,
            k_last: Option<U256>
        ) -> Self {
        UniswapV2State { 
            reserve0, 
            reserve1,
            balance0,
            balance1,
            liquidity,
            total_supply,
            k_last
        } 
    }

    //the rest of the helper methods 
    pub fn fee_to() -> Option<AlloyAddress> { //change to address
        Some(AlloyAddress::from_str("0000000000000000000000000000000000000000").unwrap())
    }

    pub fn update(
        &mut self,
        balance0: U256,
        balance1: U256,
        reserve0: U256,
        reserve1: U256,
    ) -> Result<(), SimulationError> {
        // require(balance0 <= uint112::MAX && balance1 <= uint112::MAX)
        if balance0 > U256::from(u128::MAX) || balance1 > U256::from(u128::MAX) {
            return Err(SimulationError::FatalError("overflow".to_string())); 
        }

        self.reserve0 = balance0;
        self.reserve1 = balance1;

        Ok(())
    }

    //https://github.com/Uniswap/v2-core/blob/ee547b17853e71ed4e0101ccfd52e70d5acded58/contracts/UniswapV2Pair.sol#L89
    pub fn mint_fee(
        &mut self,
        reserve0: U256,
        reserve1: U256,
        fee_to: Option<Bytes>,
    ) -> Result<bool, SimulationError> {
        let fee_on = fee_to.is_some();

        if fee_on {
            if let Some(k_last) = self.k_last {
                if !k_last.is_zero() {
                    let root_k = sqrt_u256(safe_mul_u256(reserve0, reserve1)?);
                    let root_k_last = sqrt_u256(k_last);

                    if root_k > root_k_last {
                        let numerator = safe_mul_u256(
                            self.total_supply,
                            safe_sub_u256(root_k, root_k_last)?,
                        )?;
                        let denominator = safe_add_u256(
                            safe_mul_u256(root_k, U256::from(5))?,
                            root_k_last,
                        )?;
                        let liquidity = safe_div_u256(numerator, denominator)?;

                        if liquidity > U256::ZERO {
                            self.mint(fee_to);
                        }
                    }
                }
            }
        } else if let Some(k_last) = self.k_last {
            if !k_last.is_zero() {
                self.k_last = Some(U256::ZERO);
            }
        }

        Ok(fee_on)
    }

    pub fn mint(
        &mut self,
        fee_to: Option<Bytes>,
    ) -> Result<U256, SimulationError> {
        let MINIMUM_LIQUIDITY = U256::from(1000);
        let reserve0 = self.reserve0;
        let reserve1 = self.reserve1;

        let balance0 = self.balance0;
        let balance1 = self.balance1;

        let amount0 = safe_sub_u256(balance0, reserve0)?;
        let amount1 = safe_sub_u256(balance1, reserve1)?;

        let fee_on = self.mint_fee(reserve0, reserve1, fee_to)?;

        let mut liquidity: U256;
        if self.total_supply.is_zero() {
            let prod = safe_mul_u256(amount0, amount1)?;
            liquidity = sqrt_u256(prod);
            liquidity = safe_sub_u256(liquidity, MINIMUM_LIQUIDITY)?;
            // lock minimum liquidity
            self.total_supply = safe_add_u256(self.total_supply, MINIMUM_LIQUIDITY)?;
        } else {
            let l0 = safe_div_u256(safe_mul_u256(amount0, self.total_supply)?, reserve0)?;
            let l1 = safe_div_u256(safe_mul_u256(amount1, self.total_supply)?, reserve1)?;
            liquidity = std::cmp::min(l0, l1);
        }

        if liquidity.is_zero() {
            return Err(SimulationError::FatalError("Insufficient liquidity minted".to_string())); 
        }

        // mint LP tokens
        let mut bal = self.liquidity; // this line 

        bal = safe_add_u256(bal, liquidity)?;
        self.total_supply = safe_add_u256(self.total_supply, liquidity)?;

        self.update(balance0, balance1, reserve0, reserve1)?; 
        if fee_on {
            self.k_last = Some(safe_mul_u256(self.reserve0, self.reserve1)?);
        }

        Ok(liquidity)
    }

    pub fn burn(
        &mut self,
        fee_to: Option<Bytes>, 
    ) -> Result<(U256, U256), SimulationError> {
        let reserve0 = self.reserve0;
        let reserve1 = self.reserve1;

        let balance0 = self.balance0;
        let balance1 = self.balance1;

        let liquidity = self.liquidity;

        let fee_on = self.mint_fee(reserve0, reserve1, fee_to)?;

        let amount0 = safe_div_u256(safe_mul_u256(liquidity, balance0)?, self.total_supply)?;
        let amount1 = safe_div_u256(safe_mul_u256(liquidity, balance1)?, self.total_supply)?;

        if amount0.is_zero() || amount1.is_zero() {
            return Err(SimulationError::FatalError("Insufficient liquidity burned".to_string())); 
        }

        // burn LP tokens
        self.liquidity = safe_sub_u256(liquidity, liquidity)?; //THIS
        self.total_supply = safe_sub_u256(self.total_supply, liquidity)?;

        // send tokens (state change only)
        self.balance0 = safe_sub_u256(balance0, amount0)?;

        self.balance1 = safe_sub_u256(balance1, amount1)?;

        let balance0 = self.balance0;
        let balance1 = self.balance1;

        self.update(balance0, balance1, reserve0, reserve1)?; // we don't need block timestamp so it was removed 
        if fee_on {
            self.k_last = Some(safe_mul_u256(self.reserve0, self.reserve1)?);
        }

        Ok((amount0, amount1))
    }

    //FROM: https://github.com/Uniswap/v2-periphery/blob/master/contracts/libraries/UniswapV2LiquidityMathLibrary.sol#L75
    /// Computes liquidity value given all the parameters of the pair
    pub fn compute_liquidity_value(
        &self,
        liquidity_amount: U256,
        fee_on: bool,
        k_last: Option<U256>, // we need to index k_last now lol, and fee_on 
    ) -> Result<(U256, U256), SimulationError> {
        let reserve0: U256 = self.reserve0;
        let reserve1: U256 = self.reserve1;
        let mut total_supply: U256 = self.total_supply;

        if let Some(k_last_val) = k_last {
            if fee_on && k_last_val > U256::ZERO {
                let root_k = sqrt_u256(safe_mul_u256(reserve0, reserve1)?);
                let root_k_last = sqrt_u256(k_last_val);

                if root_k > root_k_last {
                    let numerator1 = total_supply;
                    let numerator2 = safe_sub_u256(root_k, root_k_last)?;
                    let denominator = safe_add_u256(
                        safe_mul_u256(root_k, U256::from(5))?,
                        root_k_last,
                    )?;

                    let fee_liquidity = safe_div_u256(
                        safe_mul_u256(numerator1, numerator2)?,
                        denominator,
                    )?;

                    total_supply = safe_add_u256(total_supply, fee_liquidity)?;
                }
            }
        }


        let token_a_amount =
            safe_div_u256(safe_mul_u256(reserve0, liquidity_amount)?, total_supply)?;
        let token_b_amount =
            safe_div_u256(safe_mul_u256(reserve1, liquidity_amount)?, total_supply)?;

        Ok((token_a_amount, token_b_amount))
    }

    /// Gets all current parameters from the pair and computes value of a liquidity amount
    /// ⚠️ Note: subject to manipulation (e.g., sandwich attacks).
    pub fn get_liquidity_value(
        &mut self,
        liquidity_amount: U256,
    ) -> Result<(U256, U256), SimulationError> {
        let fee_on = Self::fee_to().is_some();

        let k_last = if fee_on { self.k_last } else { Some(U256::ZERO) };

        let total_supply = self.total_supply;

        Self::compute_liquidity_value(
            self,
            liquidity_amount,
            fee_on,
            k_last,
        )
    }
}

impl ProtocolSim for UniswapV2State {
    fn fee(&self) -> f64 {
        cpmm_fee(UNISWAP_V2_FEE_BPS)
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        cpmm_spot_price(base, quote, self.reserve0, self.reserve1)
    }

    fn get_amount_out( //modify this 
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let amount_in = biguint_to_u256(&amount_in);
        let zero2one = token_in.address < token_out.address;
        let amount_out = cpmm_get_amount_out(
            amount_in,
            zero2one,
            self.reserve0,
            self.reserve1,
            UNISWAP_V2_FEE_BPS,
        )?;
        let mut new_state = self.clone();
        let (reserve0_mut, reserve1_mut) = (&mut new_state.reserve0, &mut new_state.reserve1);
        if zero2one {
            *reserve0_mut = safe_add_u256(self.reserve0, amount_in)?;
            *reserve1_mut = safe_sub_u256(self.reserve1, amount_out)?;
        } else {
            *reserve0_mut = safe_sub_u256(self.reserve0, amount_out)?;
            *reserve1_mut = safe_add_u256(self.reserve1, amount_in)?;
        };
        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            120_000
                .to_biguint()
                .expect("Expected an unsigned integer as gas value"),
            Box::new(new_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        cpmm_get_limits(sell_token, buy_token, self.reserve0, self.reserve1)
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError<String>> {
        // reserve0 , reserve1, balance0, balance1, liquidity, total_supply are considered required attributes and are expected in every delta
        // we process
        let reserve0 = U256::from_be_slice(
            delta
            .updated_attributes
            .get("reserve0")
            .ok_or(TransitionError::MissingAttribute("reserve0".to_string()))?
         );
        let reserve1 = U256::from_be_slice(
            delta
                .updated_attributes
                .get("reserve1")
                .ok_or(TransitionError::MissingAttribute("reserve1".to_string()))?
        );
        let balance0 = U256::from_be_slice(
            delta
                .updated_attributes
                .get("balance0")
                .ok_or(TransitionError::MissingAttribute("reserve1".to_string()))?
        );
        let balance1 = U256::from_be_slice(
            delta
                .updated_attributes
                .get("balance1")
                .ok_or(TransitionError::MissingAttribute("reserve1".to_string()))?
        );
        let liquidity = U256::from_be_slice(
            delta
                .updated_attributes
                .get("liquidity")
                .ok_or(TransitionError::MissingAttribute("liquidity".to_string()))?
        );

        let total_supply = U256::from(10000); //PLACEHOLDER, CHANGE THIS TO USE ENTRYPOINT
                
        self.reserve0 = reserve0;
        self.reserve1 = reserve1;
        self.balance0 = balance0;
        self.balance1 = balance1;
        self.liquidity = liquidity;
        self.total_supply = total_supply;
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

   fn eq(&self, other: &dyn ProtocolSim) -> bool {
        if let Some(other_state) = other.as_any().downcast_ref::<Self>() {
            self.reserve0 == other_state.reserve0 &&
            self.reserve1 == other_state.reserve1 &&
            self.balance0 == other_state.balance0 &&
            self.balance1 == other_state.balance1 &&
            self.liquidity == other_state.liquidity &&
            self.total_supply == other_state.total_supply &&
            self.k_last == other_state.k_last &&
            self.fee() == other_state.fee()
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use approx::assert_ulps_eq;
    use num_bigint::BigUint;
    use num_traits::One;
    use rstest::rstest;
    use tycho_common::{
        dto::ProtocolStateDelta,
        hex_bytes::Bytes,
        models::{token::Token, Chain},
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{Balances, ProtocolSim},
        },
    };

    use super::*;
    use crate::evm::protocol::u256_num::biguint_to_u256;

    #[rstest]
    #[case::same_dec(
        U256::from_str("6770398782322527849696614").unwrap(),
        U256::from_str("5124813135806900540214").unwrap(),
        18,
        18,
    BigUint::from_str("10000000000000000000000").unwrap(),
    BigUint::from_str("7535635391574243447").unwrap()
    )]
    #[case::diff_dec(
        U256::from_str("33372357002392258830279").unwrap(),
        U256::from_str("43356945776493").unwrap(),
        18,
        6,
    BigUint::from_str("10000000000000000000").unwrap(),
    BigUint::from_str("12949029867").unwrap()
    )]
    fn test_get_amount_out(
        #[case] r0: U256,
        #[case] r1: U256,
        #[case] token_0_decimals: u32,
        #[case] token_1_decimals: u32,
        #[case] amount_in: BigUint,
        #[case] exp: BigUint,
    ) {
        let t0 = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
            "T0",
            token_0_decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let t1 = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "T0",
            token_1_decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let state = UniswapV2State::new(r0, r1);

        let res = state
            .get_amount_out(amount_in.clone(), &t0, &t1)
            .unwrap();

        assert_eq!(res.amount, exp);
        let new_state = res
            .new_state
            .as_any()
            .downcast_ref::<UniswapV2State>()
            .unwrap();
        assert_eq!(new_state.reserve0, r0 + biguint_to_u256(&amount_in));
        assert_eq!(new_state.reserve1, r1 - biguint_to_u256(&exp));
        // Assert that the old state is unchanged
        assert_eq!(state.reserve0, r0);
        assert_eq!(state.reserve1, r1);
    }

    #[test]
    fn test_get_amount_out_overflow() {
        let r0 = U256::from_str("33372357002392258830279").unwrap();
        let r1 = U256::from_str("43356945776493").unwrap();
        let amount_in = (BigUint::one() << 256) - BigUint::one(); // U256 max value
        let t0d = 18;
        let t1d = 16;
        let t0 = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
            "T0",
            t0d,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let t1 = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "T0",
            t1d,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let state = UniswapV2State::new(r0, r1);

        let res = state.get_amount_out(amount_in, &t0, &t1);
        assert!(res.is_err());
        let err = res.err().unwrap();
        assert!(matches!(err, SimulationError::FatalError(_)));
    }

    #[rstest]
    #[case(true, 0.0008209719947624441f64)]
    #[case(false, 1218.0683462769755f64)]
    fn test_spot_price(#[case] zero_to_one: bool, #[case] exp: f64) {
        let state = UniswapV2State::new(
            U256::from_str("36925554990922").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
        );
        let usdc = Token::new(
            &Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "USDC",
            6,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let res = if zero_to_one {
            state.spot_price(&usdc, &weth).unwrap()
        } else {
            state.spot_price(&weth, &usdc).unwrap()
        };

        assert_ulps_eq!(res, exp);
    }

    #[test]
    fn test_fee() {
        let state = UniswapV2State::new(
            U256::from_str("36925554990922").unwrap(),
            U256::from_str("30314846538607556521556").unwrap(),
        );

        let res = state.fee();

        assert_ulps_eq!(res, 0.003);
    }

    #[test]
    fn test_delta_transition() {
        let mut state =
            UniswapV2State::new(U256::from_str("1000").unwrap(), U256::from_str("1000").unwrap());//CHANGE THIS 
        let attributes: HashMap<String, Bytes> = vec![
            ("reserve0".to_string(), Bytes::from(1500_u64.to_be_bytes().to_vec())),
            ("reserve1".to_string(), Bytes::from(2000_u64.to_be_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let delta = ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(), // usv2 doesn't have any deletable attributes
        };

        let res = state.delta_transition(delta, &HashMap::new(), &Balances::default());

        assert!(res.is_ok());
        assert_eq!(state.reserve0, U256::from_str("1500").unwrap());
        assert_eq!(state.reserve1, U256::from_str("2000").unwrap());
    }

    #[test]
    fn test_delta_transition_missing_attribute() {
        let mut state =
            UniswapV2State::new(U256::from_str("1000").unwrap(), U256::from_str("1000").unwrap());
        let attributes: HashMap<String, Bytes> =
            vec![("reserve0".to_string(), Bytes::from(1500_u64.to_be_bytes().to_vec()))]
                .into_iter()
                .collect();
        let delta = ProtocolStateDelta {
            component_id: "State1".to_owned(),
            updated_attributes: attributes,
            deleted_attributes: HashSet::new(),
        };

        let res = state.delta_transition(delta, &HashMap::new(), &Balances::default());

        assert!(res.is_err());
        // assert it errors for the missing reserve1 attribute delta
        match res {
            Err(e) => {
                assert!(matches!(e, TransitionError::MissingAttribute(ref x) if x=="reserve1"))
            }
            _ => panic!("Test failed: was expecting an Err value"),
        };
    }

    #[test]
    fn test_get_limits_price_impact() {
        let state =
            UniswapV2State::new(U256::from_str("1000").unwrap(), U256::from_str("100000").unwrap());

        let (amount_in, _) = state
            .get_limits(
                Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            )
            .unwrap();

        let token_0 = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
            "T0",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let token_1 = Token::new(
            &Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            "T1",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );

        let result = state
            .get_amount_out(amount_in.clone(), &token_0, &token_1)
            .unwrap();
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<UniswapV2State>()
            .unwrap();

        let initial_price = state
            .spot_price(&token_0, &token_1)
            .unwrap();
        let new_price = new_state
            .spot_price(&token_0, &token_1)
            .unwrap()
            .floor();

        let expected_price = initial_price / 10.0;
        assert!(expected_price == new_price, "Price impact not 90%.");
    }
}
