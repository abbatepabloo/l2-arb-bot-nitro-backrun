// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20 {
    function transfer(address to,                    uint256 amount) external returns (bool);
    function balanceOf(address owner)                                 external view returns (uint256);
}

interface IUniswapV2Pair {
    function token0()      external view returns (address);
    function token1()      external view returns (address);
    function getReserves() external view returns (uint112, uint112, uint32);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

/// Minimal V2→V2 arbitrage executor used only in integration tests.
///
/// Interprets the ArbParams produced by the bot's engine / execution module:
///   tokenIn  = the token that was imbalanced in Pool A (the "cheap" token).
///   tokenOut = the paired token (the one we spend / receive profit in).
///   amountIn = optimal amount of tokenOut to route through Pool A.
///
/// The executor is pre-funded with tokenOut by the test harness; it does NOT
/// pull from msg.sender.  On success it transfers the net profit to `recipient`.
contract ArbitrageExecutor {
    struct ArbParams {
        address poolA;
        address poolB;
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint256 minProfit;
        address recipient;
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Uniswap V2 getAmountOut: dy = (997·dx·y) / (1000·x + 997·dx)
    function _amountOut(uint256 amtIn, uint256 rIn, uint256 rOut)
        internal pure returns (uint256)
    {
        uint256 withFee = amtIn * 997;
        return (withFee * rOut) / (rIn * 1000 + withFee);
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    /// Executes a two-pool arbitrage atomically.
    ///
    /// Step 1  Spend `amountIn` of tokenOut on Pool A to receive tokenIn (the cheap token).
    /// Step 2  Sell tokenIn on Pool B to receive tokenOut (at a better price).
    /// Step 3  Assert profit ≥ minProfit and forward profit to `recipient`.
    function execute(ArbParams calldata p) external {
        // ── Step 1: tokenOut → tokenIn via Pool A ─────────────────────────────

        // Route amountIn of tokenOut into Pool A.
        IERC20(p.tokenOut).transfer(p.poolA, p.amountIn);

        // Determine direction in Pool A (we are selling tokenOut, buying tokenIn).
        bool outIsToken0_A = IUniswapV2Pair(p.poolA).token0() == p.tokenOut;
        (uint112 rA0, uint112 rA1,) = IUniswapV2Pair(p.poolA).getReserves();

        uint256 gotTokenIn;
        if (outIsToken0_A) {
            // We send token0 (tokenOut), receive token1 (tokenIn).
            gotTokenIn = _amountOut(p.amountIn, rA0, rA1);
            IUniswapV2Pair(p.poolA).swap(0, gotTokenIn, address(this), "");
        } else {
            // We send token1 (tokenOut), receive token0 (tokenIn).
            gotTokenIn = _amountOut(p.amountIn, rA1, rA0);
            IUniswapV2Pair(p.poolA).swap(gotTokenIn, 0, address(this), "");
        }

        // ── Step 2: tokenIn → tokenOut via Pool B ─────────────────────────────

        IERC20(p.tokenIn).transfer(p.poolB, gotTokenIn);

        bool inIsToken0_B = IUniswapV2Pair(p.poolB).token0() == p.tokenIn;
        (uint112 rB0, uint112 rB1,) = IUniswapV2Pair(p.poolB).getReserves();

        uint256 gotTokenOut;
        if (inIsToken0_B) {
            gotTokenOut = _amountOut(gotTokenIn, rB0, rB1);
            IUniswapV2Pair(p.poolB).swap(0, gotTokenOut, address(this), "");
        } else {
            gotTokenOut = _amountOut(gotTokenIn, rB1, rB0);
            IUniswapV2Pair(p.poolB).swap(gotTokenOut, 0, address(this), "");
        }

        // ── Step 3: Profit assertion and distribution ─────────────────────────

        // gotTokenOut must exceed what we put in.
        require(
            gotTokenOut > p.amountIn + p.minProfit,
            "ARB: INSUFFICIENT_PROFIT"
        );

        // Send the gross output (amountIn is still in this contract; profit on top).
        IERC20(p.tokenOut).transfer(p.recipient, gotTokenOut);
    }
}
