// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../src/AndoMPPI.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";
import {Test} from "forge-std/Test.sol";

interface VmInvariant {
    function addr(uint256 privateKey) external returns (address);
    function prank(address caller) external;
    function sign(uint256 privateKey, bytes32 digest) external returns (uint8 v, bytes32 r, bytes32 s);
    function warp(uint256 timestamp) external;
}

contract InvariantToken {
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

contract AndoMPPIInvariantHandler {
    // forge-lint: disable-next-line(screaming-snake-case-const)
    VmInvariant private constant vm = VmInvariant(address(uint160(uint256(keccak256("hevm cheat code")))));

    // Public deterministic test fixtures. Never use or fund these keys outside tests.
    uint256 private constant PAYER_ONE_KEY = uint256(keccak256("AndoMPPI invariant payer one"));
    uint256 private constant PAYER_TWO_KEY = uint256(keccak256("AndoMPPI invariant payer two"));
    uint256 private constant SIGNER_KEY = uint256(keccak256("AndoMPPI invariant signer"));
    uint256 public constant INITIAL_BALANCE_PER_PAYER = 1_000_000_000_000_000_000_000_000;
    uint256 private constant MAX_ACTION_AMOUNT = 1_000_000_000_000;
    uint256 private constant MAX_CHANNELS = 32;

    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    address public immutable payerOne;
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    address public immutable payerTwo;
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    address public immutable signer;
    address public constant PAYEE = address(0xCAFE);
    address public constant OPERATOR = address(0xBEEF);

    // Lowercase public names intentionally expose readable handler getters.
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    AndoMPPI public immutable escrow;
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    InvariantToken public immutable usdc;
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    InvariantToken public immutable eurc;

    struct ChannelModel {
        bytes32 channelId;
        address payer;
        address token;
        address operator;
        uint128 deposit;
        uint128 settled;
        uint64 closeRequestedAt;
        bool finalized;
    }

    ChannelModel[] private channelModels;
    uint256 private nextSalt;

    constructor() {
        payerOne = vm.addr(PAYER_ONE_KEY);
        payerTwo = vm.addr(PAYER_TWO_KEY);
        signer = vm.addr(SIGNER_KEY);
        usdc = new InvariantToken();
        eurc = new InvariantToken();

        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(usdc), address(eurc), address(this), address(this)));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        escrow = AndoMPPI(address(proxy));

        _fundAndApprove(payerOne);
        _fundAndApprove(payerTwo);
    }

    function open(uint256 payerSeed, uint256 tokenSeed, uint128 depositSeed, bool withOperator) external {
        if (escrow.paused() || channelModels.length >= MAX_CHANNELS) return;

        address payer = payerSeed % 2 == 0 ? payerOne : payerTwo;
        address token = tokenSeed % 2 == 0 ? address(usdc) : address(eurc);
        address operator = withOperator ? OPERATOR : address(0);
        // The modulo upper bound is far below uint128.max.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint128 deposit = uint128(uint256(depositSeed) % MAX_ACTION_AMOUNT) + 1;
        bytes32 salt = keccak256(abi.encode(++nextSalt, payer, token));

        vm.prank(payer);
        bytes32 channelId = withOperator
            ? escrow.openWithOperator(PAYEE, operator, token, deposit, salt, signer)
            : escrow.open(PAYEE, token, deposit, salt, signer);

        channelModels.push(
            ChannelModel({
                channelId: channelId,
                payer: payer,
                token: token,
                operator: operator,
                deposit: deposit,
                settled: 0,
                closeRequestedAt: 0,
                finalized: false
            })
        );
    }

    function topUp(uint256 channelSeed, uint128 amountSeed) external {
        if (escrow.paused() || channelModels.length == 0) return;
        ChannelModel storage model = channelModels[channelSeed % channelModels.length];
        if (model.finalized) return;

        // The modulo upper bound is far below uint128.max.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint128 amount = uint128(uint256(amountSeed) % MAX_ACTION_AMOUNT) + 1;
        vm.prank(model.payer);
        escrow.topUp(model.channelId, amount);
        model.deposit += amount;
        model.closeRequestedAt = 0;
    }

    function settle(uint256 channelSeed, uint128 amountSeed, bool useOperator) external {
        if (channelModels.length == 0) return;
        ChannelModel storage model = channelModels[channelSeed % channelModels.length];
        if (model.finalized || model.settled == model.deposit) return;

        uint128 available = model.deposit - model.settled;
        uint128 delta = uint128(uint256(amountSeed) % available) + 1;
        uint128 cumulativeAmount = model.settled + delta;
        bytes memory voucher = _voucher(model.channelId, cumulativeAmount);
        address caller = useOperator && model.operator != address(0) ? model.operator : PAYEE;

        vm.prank(caller);
        escrow.settle(model.channelId, cumulativeAmount, voucher);
        model.settled = cumulativeAmount;
    }

    function requestClose(uint256 channelSeed) external {
        if (channelModels.length == 0) return;
        ChannelModel storage model = channelModels[channelSeed % channelModels.length];
        if (model.finalized) return;

        vm.prank(model.payer);
        escrow.requestClose(model.channelId);
        model.closeRequestedAt = escrow.getChannel(model.channelId).closeRequestedAt;
    }

    function close(uint256 channelSeed, uint128 amountSeed, bool useOperator) external {
        if (channelModels.length == 0) return;
        ChannelModel storage model = channelModels[channelSeed % channelModels.length];
        if (model.finalized) return;

        uint128 available = model.deposit - model.settled;
        uint128 delta = available == 0 ? 0 : uint128(uint256(amountSeed) % (uint256(available) + 1));
        uint128 cumulativeAmount = model.settled + delta;
        bytes memory voucher = delta == 0 ? bytes("") : _voucher(model.channelId, cumulativeAmount);
        address caller = useOperator && model.operator != address(0) ? model.operator : PAYEE;

        vm.prank(caller);
        escrow.close(model.channelId, cumulativeAmount, voucher);
        model.settled = cumulativeAmount;
        model.closeRequestedAt = 0;
        model.finalized = true;
    }

    function withdraw(uint256 channelSeed) external {
        if (channelModels.length == 0) return;
        ChannelModel storage model = channelModels[channelSeed % channelModels.length];
        if (model.finalized || model.closeRequestedAt == 0) return;

        uint256 readyAt = uint256(model.closeRequestedAt) + escrow.CLOSE_GRACE_PERIOD();
        if (block.timestamp < readyAt) vm.warp(readyAt);
        vm.prank(model.payer);
        escrow.withdraw(model.channelId);
        model.closeRequestedAt = 0;
        model.finalized = true;
    }

    function toggleFundingPause() external {
        if (escrow.paused()) {
            escrow.unpauseFunding();
        } else {
            escrow.pauseFunding();
        }
    }

    function advanceTime(uint32 secondsSeed) external {
        vm.warp(block.timestamp + (uint256(secondsSeed) % 1 days));
    }

    function channelCount() external view returns (uint256) {
        return channelModels.length;
    }

    function channelModel(uint256 index) external view returns (ChannelModel memory) {
        return channelModels[index];
    }

    function _fundAndApprove(address payer) private {
        usdc.mint(payer, INITIAL_BALANCE_PER_PAYER);
        eurc.mint(payer, INITIAL_BALANCE_PER_PAYER);
        vm.prank(payer);
        usdc.approve(address(escrow), type(uint256).max);
        vm.prank(payer);
        eurc.approve(address(escrow), type(uint256).max);
    }

    function _voucher(bytes32 channelId, uint128 cumulativeAmount) private returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(SIGNER_KEY, escrow.voucherDigest(channelId, cumulativeAmount));
        return abi.encodePacked(r, s, v);
    }
}

contract AndoMPPIInvariantTest is Test {
    AndoMPPIInvariantHandler private handler;

    function setUp() public {
        handler = new AndoMPPIInvariantHandler();
        targetContract(address(handler));
    }

    function testInvariantHarnessTargetsDeployedHandler() public view {
        require(address(handler) != address(0), "handler not deployed");
        require(targetContracts().length == 1, "handler not targeted");
        require(targetContracts()[0] == address(handler), "wrong invariant target");
    }

    function invariant_channelStateMatchesIndependentModel() external view {
        uint256 count = handler.channelCount();
        for (uint256 index; index < count; ++index) {
            AndoMPPIInvariantHandler.ChannelModel memory model = handler.channelModel(index);
            AndoMPPI.Channel memory channel = handler.escrow().getChannel(model.channelId);

            require(channel.payer == model.payer, "payer drift");
            require(channel.payee == handler.PAYEE(), "payee drift");
            require(channel.operator == model.operator, "operator drift");
            require(channel.token == model.token, "currency drift");
            require(channel.authorizedSigner == handler.signer(), "signer drift");
            require(channel.deposit == model.deposit, "deposit drift");
            require(channel.settled == model.settled, "settlement drift");
            require(channel.closeRequestedAt == model.closeRequestedAt, "close timer drift");
            require(channel.finalized == model.finalized, "finalized drift");
            require(channel.settled <= channel.deposit, "settlement exceeds deposit");
            if (channel.finalized) require(channel.closeRequestedAt == 0, "finalized timer retained");
        }
    }

    function invariant_escrowBalancesEqualOutstandingChannelLiabilities() external view {
        uint256 usdcLiability;
        uint256 eurcLiability;
        uint256 count = handler.channelCount();

        for (uint256 index; index < count; ++index) {
            AndoMPPIInvariantHandler.ChannelModel memory model = handler.channelModel(index);
            if (model.finalized) continue;
            uint256 liability = uint256(model.deposit) - model.settled;
            if (model.token == address(handler.usdc())) {
                usdcLiability += liability;
            } else {
                eurcLiability += liability;
            }
        }

        require(handler.usdc().balanceOf(address(handler.escrow())) == usdcLiability, "USDC insolvency");
        require(handler.eurc().balanceOf(address(handler.escrow())) == eurcLiability, "EURC insolvency");
    }

    function invariant_tokenSupplyIsConservedAcrossAllParticipants() external view {
        uint256 expectedSupply = handler.INITIAL_BALANCE_PER_PAYER() * 2;
        uint256 usdcObserved = handler.usdc().balanceOf(handler.payerOne())
            + handler.usdc().balanceOf(handler.payerTwo()) + handler.usdc().balanceOf(handler.PAYEE())
            + handler.usdc().balanceOf(address(handler.escrow()));
        uint256 eurcObserved = handler.eurc().balanceOf(handler.payerOne())
            + handler.eurc().balanceOf(handler.payerTwo()) + handler.eurc().balanceOf(handler.PAYEE())
            + handler.eurc().balanceOf(address(handler.escrow()));

        require(usdcObserved == expectedSupply, "USDC conservation failure");
        require(eurcObserved == expectedSupply, "EURC conservation failure");
        require(handler.usdc().balanceOf(handler.OPERATOR()) == 0, "operator received USDC");
        require(handler.eurc().balanceOf(handler.OPERATOR()) == 0, "operator received EURC");
    }
}
