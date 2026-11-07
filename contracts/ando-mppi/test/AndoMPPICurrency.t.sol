// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../src/AndoMPPI.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";

interface VmCurrency {
    function addr(uint256 privateKey) external returns (address);
    function expectRevert(bytes4 revertData) external;
    function prank(address caller) external;
}

interface IAndoMPPICurrency {
    function open(address payee, address token, uint128 deposit, bytes32 salt, address authorizedSigner)
        external
        returns (bytes32 channelId);
}

contract CurrencyToken {
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

contract FeeOnTransferToken {
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
        balanceOf[recipient] += amount - (amount / 100);
        return true;
    }

    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool) {
        allowance[sender][msg.sender] -= amount;
        balanceOf[sender] -= amount;
        balanceOf[recipient] += amount - (amount / 100);
        return true;
    }
}

contract AndoMPPICurrencyTest {
    // forge-lint: disable-next-line(screaming-snake-case-const)
    VmCurrency private constant vm = VmCurrency(address(uint160(uint256(keccak256("hevm cheat code")))));

    // Public deterministic test fixture. Never use or fund this key outside tests.
    uint256 private constant PAYER_KEY = uint256(keccak256("AndoMPPI currency test payer"));
    address private constant PAYEE = address(0xCAFE);
    CurrencyToken private usdc;
    CurrencyToken private eurc;
    IAndoMPPICurrency private escrow;
    address private payer;

    function setUp() public {
        payer = vm.addr(PAYER_KEY);
        usdc = new CurrencyToken();
        eurc = new CurrencyToken();
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization = abi.encodeWithSignature(
            "initialize(address,address,address,address)", usdc, eurc, address(this), address(1)
        );
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        escrow = IAndoMPPICurrency(address(proxy));

        usdc.mint(payer, 2_000_000);
        eurc.mint(payer, 2_000_000);
        vm.prank(payer);
        usdc.approve(address(escrow), type(uint256).max);
        vm.prank(payer);
        eurc.approve(address(escrow), type(uint256).max);
    }

    function testOpenAcceptsBothConfiguredCurrencies() public {
        vm.prank(payer);
        bytes32 usdcChannel = escrow.open(PAYEE, address(usdc), 1_000_000, bytes32(uint256(1)), address(0));
        require(usdcChannel != bytes32(0), "USDC channel was not opened");

        vm.prank(payer);
        bytes32 eurcChannel = escrow.open(PAYEE, address(eurc), 1_000_000, bytes32(uint256(2)), address(0));
        require(eurcChannel != bytes32(0), "EURC channel was not opened");
    }

    function testInitializerRejectsDuplicateCurrenciesAsInvalidCurrency() public {
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(usdc), address(usdc), address(this), address(this)));

        vm.expectRevert(AndoMPPI.InvalidCurrency.selector);
        new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
    }

    function testInitializerRejectsZeroGovernance() public {
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(usdc), address(eurc), address(0), address(this)));

        vm.expectRevert(AndoMPPI.ZeroAddress.selector);
        new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
    }

    function testOpenRejectsEveryUnconfiguredCurrency() public {
        CurrencyToken unsupported = new CurrencyToken();
        unsupported.mint(payer, 1_000_000);
        vm.prank(payer);
        unsupported.approve(address(escrow), type(uint256).max);

        vm.prank(payer);
        (bool opened,) = address(escrow)
            .call(
                abi.encodeCall(
                    IAndoMPPICurrency.open,
                    (PAYEE, address(unsupported), uint128(1_000_000), bytes32(uint256(3)), address(0))
                )
            );
        require(!opened, "unsupported currency was accepted");
    }

    function testExactTransferCheckRejectsFeeOnTransferBehavior() public {
        FeeOnTransferToken feeToken = new FeeOnTransferToken();
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(feeToken), address(eurc), address(this), address(this)));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        AndoMPPI feeEscrow = AndoMPPI(address(proxy));
        feeToken.mint(payer, 1_000_000);
        vm.prank(payer);
        feeToken.approve(address(feeEscrow), type(uint256).max);

        vm.prank(payer);
        (bool opened,) = address(feeEscrow)
            .call(
                abi.encodeCall(
                    AndoMPPI.open, (PAYEE, address(feeToken), uint128(1_000_000), bytes32(uint256(4)), address(0))
                )
            );

        require(!opened, "fee-on-transfer token created an underfunded channel");
        require(feeToken.balanceOf(address(feeEscrow)) == 0, "failed open retained funds");
    }
}
