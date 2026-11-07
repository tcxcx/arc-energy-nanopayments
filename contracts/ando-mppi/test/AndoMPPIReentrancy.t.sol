// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../src/AndoMPPI.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";
import {Test} from "forge-std/Test.sol";

interface ITokenTransferCallback {
    function onTokenTransfer() external;
}

/// @dev Adversarial ERC-20 that invokes recipient code during a transfer.
contract CallbackToken {
    mapping(address account => uint256 balance) public balanceOf;
    mapping(address owner => mapping(address spender => uint256 amount)) public allowance;
    bool public lastCallbackSucceeded;

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
        if (recipient.code.length != 0) {
            // The malicious token deliberately ignores a failed callback so the
            // outer ERC-20 transfer remains standards-compatible.
            (lastCallbackSucceeded,) =
                address(recipient).call(abi.encodeCall(ITokenTransferCallback.onTokenTransfer, ()));
        }
        return true;
    }

    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool) {
        allowance[sender][msg.sender] -= amount;
        balanceOf[sender] -= amount;
        balanceOf[recipient] += amount;
        return true;
    }
}

contract ReentrantPayee is ITokenTransferCallback {
    AndoMPPI private immutable ESCROW;
    CallbackToken private immutable TOKEN;
    bytes32 private channelId;
    uint128 private reentrantAmount;
    bytes private reentrantVoucher;

    bool public callbackAttempted;
    bool public reentrySucceeded;

    constructor(AndoMPPI escrow_, CallbackToken token_) {
        ESCROW = escrow_;
        TOKEN = token_;
    }

    function configure(bytes32 channelId_, uint128 amount_, bytes calldata voucher_) external {
        channelId = channelId_;
        reentrantAmount = amount_;
        reentrantVoucher = voucher_;
    }

    function settleFirst(uint128 amount, bytes calldata voucher) external {
        ESCROW.settle(channelId, amount, voucher);
    }

    function onTokenTransfer() external {
        require(msg.sender == address(TOKEN), "unexpected callback");
        callbackAttempted = true;
        (reentrySucceeded,) =
            address(ESCROW).call(abi.encodeCall(AndoMPPI.settle, (channelId, reentrantAmount, reentrantVoucher)));
    }
}

contract AndoMPPIReentrancyTest is Test {
    // Public deterministic test fixtures. Never use or fund these keys outside tests.
    uint256 private constant PAYER_KEY = uint256(keccak256("AndoMPPI reentrancy test payer"));
    uint256 private constant SIGNER_KEY = uint256(keccak256("AndoMPPI reentrancy test signer"));

    address private payer;
    address private signer;
    AndoMPPI private escrow;
    CallbackToken private token;
    CallbackToken private eurc;
    ReentrantPayee private payee;

    function setUp() public {
        payer = vm.addr(PAYER_KEY);
        signer = vm.addr(SIGNER_KEY);
        token = new CallbackToken();
        eurc = new CallbackToken();

        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(token), address(eurc), address(this), address(this)));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        escrow = AndoMPPI(address(proxy));
        payee = new ReentrantPayee(escrow, token);

        token.mint(payer, 5_000_000);
        vm.prank(payer);
        token.approve(address(escrow), type(uint256).max);
    }

    function testMaliciousTokenCannotReenterSettlement() public {
        bytes32 salt = bytes32(uint256(1));
        vm.prank(payer);
        bytes32 channelId = escrow.open(address(payee), address(token), 5_000_000, salt, signer);

        bytes memory firstVoucher = _voucher(channelId, 1_000_000);
        bytes memory reentrantVoucher = _voucher(channelId, 2_000_000);
        payee.configure(channelId, 2_000_000, reentrantVoucher);
        payee.settleFirst(1_000_000, firstVoucher);

        assertTrue(payee.callbackAttempted(), "malicious callback was not exercised");
        assertFalse(payee.reentrySucceeded(), "reentrant settlement succeeded");
        assertEq(escrow.getChannel(channelId).settled, 1_000_000, "reentry changed cumulative settlement");
        assertEq(token.balanceOf(address(payee)), 1_000_000, "reentry paid the provider twice");
        assertEq(token.balanceOf(address(escrow)), 4_000_000, "escrow liability changed during reentry");
    }

    function _voucher(bytes32 channelId, uint128 cumulativeAmount) private view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(SIGNER_KEY, escrow.voucherDigest(channelId, cumulativeAmount));
        return abi.encodePacked(r, s, v);
    }
}
