// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../src/AndoMPPI.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";
import {Test} from "forge-std/Test.sol";

contract GasToken {
    mapping(address account => uint256 balance) public balanceOf;
    mapping(address owner => mapping(address spender => uint256 amount)) public allowance;

    function mint(address account, uint256 amount) external {
        balanceOf[account] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address recipient, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[recipient] += amount;
        return true;
    }

    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool) {
        allowance[sender][msg.sender] -= amount;
        balanceOf[sender] -= amount;
        balanceOf[recipient] += amount;
        return true;
    }
}

contract AndoMPPIGasTest is Test {
    // Public deterministic test fixtures. Never use or fund these keys outside tests.
    uint256 private constant PAYER_KEY = uint256(keccak256("AndoMPPI gas test payer"));
    uint256 private constant SIGNER_KEY = uint256(keccak256("AndoMPPI gas test signer"));
    address private constant PAYEE = address(0xCAFE);
    address private constant OPERATOR = address(0xBEEF);

    // Deliberate regression ceilings, not exact gas promises. Snapshots capture
    // precise compiler-specific movement while these bounds stop major regressions.
    uint256 private constant OPEN_GAS_CEILING = 280_000;
    uint256 private constant OPEN_WITH_OPERATOR_GAS_CEILING = 300_000;
    uint256 private constant TOP_UP_GAS_CEILING = 30_000;
    uint256 private constant SETTLE_GAS_CEILING = 55_000;
    uint256 private constant SETTLE_BATCH_TWO_GAS_CEILING = 100_000;
    uint256 private constant REQUEST_CLOSE_GAS_CEILING = 35_000;
    uint256 private constant CLOSE_GAS_CEILING = 85_000;
    uint256 private constant WITHDRAW_GAS_CEILING = 20_000;

    address private payer;
    address private signer;
    AndoMPPI private escrow;
    GasToken private usdc;
    GasToken private eurc;
    uint256 private nextSalt;

    function setUp() public {
        payer = vm.addr(PAYER_KEY);
        signer = vm.addr(SIGNER_KEY);
        usdc = new GasToken();
        eurc = new GasToken();
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(usdc), address(eurc), address(this), address(this)));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        escrow = AndoMPPI(address(proxy));

        usdc.mint(payer, 100_000_000);
        vm.prank(payer);
        usdc.approve(address(escrow), type(uint256).max);
    }

    function testGasOpenStaysWithinCeiling() public {
        vm.prank(payer);
        uint256 gasBefore = gasleft();
        escrow.open(PAYEE, address(usdc), 5_000_000, _salt(), signer);
        _assertGasBelow("open", gasBefore - gasleft(), OPEN_GAS_CEILING);
    }

    function testGasOpenWithOperatorStaysWithinCeiling() public {
        vm.prank(payer);
        uint256 gasBefore = gasleft();
        escrow.openWithOperator(PAYEE, OPERATOR, address(usdc), 5_000_000, _salt(), signer);
        _assertGasBelow("openWithOperator", gasBefore - gasleft(), OPEN_WITH_OPERATOR_GAS_CEILING);
    }

    function testGasTopUpStaysWithinCeiling() public {
        bytes32 channelId = _open();
        vm.prank(payer);
        uint256 gasBefore = gasleft();
        escrow.topUp(channelId, 1_000_000);
        _assertGasBelow("topUp", gasBefore - gasleft(), TOP_UP_GAS_CEILING);
    }

    function testGasSettleStaysWithinCeiling() public {
        bytes32 channelId = _open();
        bytes memory voucher = _voucher(channelId, 1_000_000);
        vm.prank(PAYEE);
        uint256 gasBefore = gasleft();
        escrow.settle(channelId, 1_000_000, voucher);
        _assertGasBelow("settle", gasBefore - gasleft(), SETTLE_GAS_CEILING);
    }

    function testGasSettleBatchOfTwoStaysWithinCeiling() public {
        bytes32 first = _open();
        bytes32 second = _open();
        AndoMPPI.Settlement[] memory settlements = new AndoMPPI.Settlement[](2);
        settlements[0] =
            AndoMPPI.Settlement({channelId: first, cumulativeAmount: 1_000_000, signature: _voucher(first, 1_000_000)});
        settlements[1] = AndoMPPI.Settlement({
            channelId: second, cumulativeAmount: 1_000_000, signature: _voucher(second, 1_000_000)
        });

        vm.prank(PAYEE);
        uint256 gasBefore = gasleft();
        escrow.settleBatch(settlements);
        _assertGasBelow("settleBatch(2)", gasBefore - gasleft(), SETTLE_BATCH_TWO_GAS_CEILING);
    }

    function testGasRequestCloseStaysWithinCeiling() public {
        bytes32 channelId = _open();
        vm.prank(payer);
        uint256 gasBefore = gasleft();
        escrow.requestClose(channelId);
        _assertGasBelow("requestClose", gasBefore - gasleft(), REQUEST_CLOSE_GAS_CEILING);
    }

    function testGasCooperativeCloseStaysWithinCeiling() public {
        bytes32 channelId = _open();
        bytes memory voucher = _voucher(channelId, 1_000_000);
        vm.prank(PAYEE);
        uint256 gasBefore = gasleft();
        escrow.close(channelId, 1_000_000, voucher);
        _assertGasBelow("close", gasBefore - gasleft(), CLOSE_GAS_CEILING);
    }

    function testGasForcedWithdrawStaysWithinCeiling() public {
        bytes32 channelId = _open();
        vm.prank(payer);
        escrow.requestClose(channelId);
        vm.warp(block.timestamp + escrow.CLOSE_GRACE_PERIOD());

        vm.prank(payer);
        uint256 gasBefore = gasleft();
        escrow.withdraw(channelId);
        _assertGasBelow("withdraw", gasBefore - gasleft(), WITHDRAW_GAS_CEILING);
    }

    function _open() private returns (bytes32 channelId) {
        bytes32 salt = _salt();
        vm.prank(payer);
        channelId = escrow.open(PAYEE, address(usdc), 5_000_000, salt, signer);
    }

    function _voucher(bytes32 channelId, uint128 cumulativeAmount) private view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(SIGNER_KEY, escrow.voucherDigest(channelId, cumulativeAmount));
        return abi.encodePacked(r, s, v);
    }

    function _salt() private returns (bytes32) {
        return bytes32(++nextSalt);
    }

    function _assertGasBelow(string memory operation, uint256 gasUsed, uint256 ceiling) private {
        emit log_named_uint(operation, gasUsed);
        assertLt(gasUsed, ceiling, string.concat(operation, " gas regression"));
    }
}
