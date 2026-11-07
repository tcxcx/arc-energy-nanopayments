// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../../src/AndoMPPI.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IERC20Metadata} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {Test} from "forge-std/Test.sol";

/// @notice Read-only Arc Testnet interoperability checks.
/// @dev Runs on a local fork only and never broadcasts a transaction.
contract AndoMPPIArcForkTest is Test {
    uint256 private constant ARC_TESTNET_CHAIN_ID = 5_042_002;
    uint256 private constant ARC_TESTNET_FORK_BLOCK = 54_275_166;
    address private constant ARC_TESTNET_USDC = 0x3600000000000000000000000000000000000000;
    address private constant ARC_TESTNET_EURC = 0x89B50855Aa3bE2F677cD6303Cec089B5F319D72a;
    // Public deterministic test fixtures. Never use or fund these keys outside tests.
    uint256 private constant PAYER_KEY = uint256(keccak256("AndoMPPI Arc fork test payer"));
    uint256 private constant SIGNER_KEY = uint256(keccak256("AndoMPPI Arc fork test signer"));
    address private constant PAYEE = address(0xCAFE);

    address private payer;
    address private signer;
    AndoMPPI private escrow;

    function setUp() public {
        if (!vm.envOr("RUN_ARC_FORK", false)) {
            vm.skip(true);
            return;
        }

        string memory rpcUrl = vm.envOr("ARC_TESTNET_RPC_URL", string("https://rpc.testnet.arc.io"));
        vm.createSelectFork(rpcUrl, ARC_TESTNET_FORK_BLOCK);
        payer = vm.addr(PAYER_KEY);
        signer = vm.addr(SIGNER_KEY);

        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (ARC_TESTNET_USDC, ARC_TESTNET_EURC, address(this), address(this)));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        escrow = AndoMPPI(address(proxy));
    }

    function testOfficialArcCurrencyConfiguration() public view {
        assertEq(block.chainid, ARC_TESTNET_CHAIN_ID, "unexpected Arc chain");
        assertGt(ARC_TESTNET_USDC.code.length, 0, "official USDC has no code");
        assertGt(ARC_TESTNET_EURC.code.length, 0, "official EURC has no code");
        assertEq(IERC20Metadata(ARC_TESTNET_USDC).decimals(), 6, "unexpected USDC decimals");
        assertEq(IERC20Metadata(ARC_TESTNET_EURC).decimals(), 6, "unexpected EURC decimals");
        assertEq(
            keccak256(bytes(IERC20Metadata(ARC_TESTNET_USDC).symbol())),
            keccak256(bytes("USDC")),
            "unexpected USDC symbol"
        );
        assertEq(
            keccak256(bytes(IERC20Metadata(ARC_TESTNET_EURC).symbol())),
            keccak256(bytes("EURC")),
            "unexpected EURC symbol"
        );
        assertEq(escrow.usdc(), ARC_TESTNET_USDC, "wrong configured USDC");
        assertEq(escrow.eurc(), ARC_TESTNET_EURC, "wrong configured EURC");
    }

    function testOfficialArcUSDCApprovalCompatibility() public {
        uint256 allowance = 6_000_000;

        vm.prank(payer);
        assertTrue(IERC20(ARC_TESTNET_USDC).approve(address(escrow), allowance), "USDC approval failed");

        assertEq(IERC20(ARC_TESTNET_USDC).allowance(payer, address(escrow)), allowance, "USDC allowance mismatch");
    }

    function testLifecycleAgainstOfficialArcEURC() public {
        _exerciseLifecycle(ARC_TESTNET_EURC, bytes32(uint256(2)));
    }

    function _exerciseLifecycle(address token, bytes32 salt) private {
        uint128 initialDeposit = 5_000_000;
        uint128 topUpAmount = 1_000_000;
        uint128 cumulativeAmount = 2_000_000;
        address fundingPayer = payer;
        deal(token, fundingPayer, initialDeposit + topUpAmount, true);
        uint256 payerBalanceBefore = IERC20(token).balanceOf(fundingPayer);
        uint256 payeeBalanceBefore = IERC20(token).balanceOf(PAYEE);
        assertGe(payerBalanceBefore, initialDeposit + topUpAmount, "fork funding failed");

        vm.startPrank(fundingPayer);
        IERC20(token).approve(address(escrow), type(uint256).max);
        bytes32 channelId = escrow.open(PAYEE, token, initialDeposit, salt, signer);
        escrow.topUp(channelId, topUpAmount);
        vm.stopPrank();

        bytes memory voucher = _voucher(channelId, cumulativeAmount);
        vm.prank(PAYEE);
        escrow.close(channelId, cumulativeAmount, voucher);

        AndoMPPI.Channel memory channel = escrow.getChannel(channelId);
        assertTrue(channel.finalized, "channel did not finalize");
        assertEq(channel.deposit, initialDeposit + topUpAmount, "deposit mismatch");
        assertEq(channel.settled, cumulativeAmount, "settlement mismatch");
        assertEq(IERC20(token).balanceOf(PAYEE), payeeBalanceBefore + cumulativeAmount, "provider payment mismatch");
        assertEq(IERC20(token).balanceOf(fundingPayer), payerBalanceBefore - cumulativeAmount, "payer refund mismatch");
        assertEq(IERC20(token).balanceOf(address(escrow)), 0, "escrow retained funds");
    }

    function _voucher(bytes32 channelId, uint128 cumulativeAmount) private view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(SIGNER_KEY, escrow.voucherDigest(channelId, cumulativeAmount));
        return abi.encodePacked(r, s, v);
    }
}
