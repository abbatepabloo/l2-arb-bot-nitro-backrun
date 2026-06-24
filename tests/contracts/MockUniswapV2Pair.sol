// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20 {
    function transfer(address to,     uint256 amount) external returns (bool);
    function balanceOf(address owner)                 external view returns (uint256);
}

/// Minimal Uniswap-V2-compatible pair used only in integration tests.
///
/// Supports the subset of the V2 interface the bot and executor actually call:
///   getReserves(), token0(), token1(), swap(), setReserves() (test helper).
contract MockUniswapV2Pair {
    address public token0;
    address public token1;

    uint112 private _reserve0;
    uint112 private _reserve1;
    uint32  private _blockTimestampLast;

    event Sync(uint112 reserve0, uint112 reserve1);
    event Swap(
        address indexed sender,
        uint256 amount0In,  uint256 amount1In,
        uint256 amount0Out, uint256 amount1Out,
        address indexed to
    );

    // ── Initialisation (replaces factory) ────────────────────────────────────

    /// Called once after deployment instead of a factory.
    function initialize(address _token0, address _token1) external {
        require(token0 == address(0), "already initialised");
        // Respect the V2 invariant: token0 is the lower address.
        if (_token0 < _token1) { token0 = _token0; token1 = _token1; }
        else                   { token0 = _token1; token1 = _token0; }
    }

    // ── Test helper ───────────────────────────────────────────────────────────

    /// Allow the test to set reserves without doing an actual swap.
    /// Used to bootstrap imbalanced pool state for the arb scenario.
    function setReserves(uint112 r0, uint112 r1) external {
        _reserve0 = r0;
        _reserve1 = r1;
        _blockTimestampLast = uint32(block.timestamp);
        emit Sync(r0, r1);
    }

    /// Deposit tokens and record reserves (used for initial liquidity setup).
    function sync() external {
        _reserve0 = uint112(IERC20(token0).balanceOf(address(this)));
        _reserve1 = uint112(IERC20(token1).balanceOf(address(this)));
        _blockTimestampLast = uint32(block.timestamp);
        emit Sync(_reserve0, _reserve1);
    }

    // ── V2 interface ──────────────────────────────────────────────────────────

    function getReserves()
        external view
        returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast)
    {
        return (_reserve0, _reserve1, _blockTimestampLast);
    }

    /// Standard V2 swap.  Caller must transfer the input token to this contract
    /// before calling (flash-swap callbacks are forwarded but not required).
    function swap(
        uint256 amount0Out,
        uint256 amount1Out,
        address to,
        bytes calldata /* data */
    ) external {
        require(amount0Out > 0 || amount1Out > 0, "V2: INSUF_OUTPUT");
        uint112 r0 = _reserve0;
        uint112 r1 = _reserve1;
        require(amount0Out < r0 && amount1Out < r1, "V2: INSUF_LIQ");

        if (amount0Out > 0) IERC20(token0).transfer(to, amount0Out);
        if (amount1Out > 0) IERC20(token1).transfer(to, amount1Out);

        uint256 bal0 = IERC20(token0).balanceOf(address(this));
        uint256 bal1 = IERC20(token1).balanceOf(address(this));

        // Derive inputs: how many tokens came IN since last reserves.
        uint256 in0 = bal0 > r0 - amount0Out ? bal0 - (r0 - amount0Out) : 0;
        uint256 in1 = bal1 > r1 - amount1Out ? bal1 - (r1 - amount1Out) : 0;
        require(in0 > 0 || in1 > 0, "V2: INSUF_INPUT");

        // Verify constant-product with 0.3 % fee.
        uint256 adj0 = bal0 * 1000 - in0 * 3;
        uint256 adj1 = bal1 * 1000 - in1 * 3;
        require(
            adj0 * adj1 >= uint256(r0) * uint256(r1) * 1_000_000,
            "V2: K"
        );

        _reserve0 = uint112(bal0);
        _reserve1 = uint112(bal1);
        _blockTimestampLast = uint32(block.timestamp);

        emit Sync(_reserve0, _reserve1);
        emit Swap(msg.sender, in0, in1, amount0Out, amount1Out, to);
    }
}
