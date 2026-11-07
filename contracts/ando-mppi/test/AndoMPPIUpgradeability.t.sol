// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../src/AndoMPPI.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";
import {ITransparentUpgradeableProxy, ProxyAdmin} from "@openzeppelin/contracts/proxy/transparent/ProxyAdmin.sol";
import {TimelockController} from "@openzeppelin/contracts/governance/TimelockController.sol";

contract ContractAddress {}

interface VmUpgrade {
    function addr(uint256 privateKey) external returns (address);
    function load(address target, bytes32 slot) external view returns (bytes32);
    function prank(address caller) external;
    function sign(uint256 privateKey, bytes32 digest) external returns (uint8 v, bytes32 r, bytes32 s);
    function warp(uint256 timestamp) external;
}

contract UpgradeToken {
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

contract AndoMPPIUpgradeV2 is AndoMPPI {
    function upgradeMarker() external pure returns (uint256) {
        return 2;
    }
}

contract AndoMPPIUpgradeabilityTest {
    // forge-lint: disable-next-line(screaming-snake-case-const)
    VmUpgrade private constant vm = VmUpgrade(address(uint160(uint256(keccak256("hevm cheat code")))));
    bytes32 private constant ERC1967_ADMIN_SLOT = bytes32(uint256(keccak256("eip1967.proxy.admin")) - 1);
    // Public deterministic test fixture. Never use or fund this key outside tests.
    uint256 private constant SIGNER_KEY = uint256(keccak256("AndoMPPI upgrade test signer"));

    struct UpgradeSnapshot {
        AndoMPPI.Channel active;
        AndoMPPI.Channel closing;
        AndoMPPI.Channel finalized;
        bytes32 domainSeparator;
        uint256 usdcEscrowBalance;
        uint256 eurcEscrowBalance;
    }

    struct UpgradeContext {
        UpgradeToken usdc;
        UpgradeToken eurc;
        AndoMPPI escrow;
        TransparentUpgradeableProxy proxy;
        bytes32 active;
        bytes32 closing;
        bytes32 finalized;
    }

    function testImplementationDeploysWithoutRuntimeConfiguration() public {
        address implementation = _deploy(type(AndoMPPI).creationCode);
        require(implementation != address(0), "implementation deployment failed");
    }

    function testProxyInitializesExactlyOnceAndImplementationStaysLocked() public {
        AndoMPPI implementation = new AndoMPPI();
        ContractAddress usdc = new ContractAddress();
        ContractAddress eurc = new ContractAddress();
        bytes memory initialization = abi.encodeWithSignature(
            "initialize(address,address,address,address)", usdc, eurc, address(this), address(1)
        );
        address proxy = _deploy(
            abi.encodePacked(
                type(TransparentUpgradeableProxy).creationCode,
                abi.encode(address(implementation), address(this), initialization)
            )
        );
        require(proxy != address(0), "proxy initialization failed");

        (bool initializedTwice,) = proxy.call(initialization);
        require(!initializedTwice, "proxy initialized twice");

        (bool implementationInitialized,) = address(implementation).call(initialization);
        require(!implementationInitialized, "implementation was not locked");
    }

    function testOnlyProxyAdminOwnerUpgradesAndChannelStorageSurvives() public {
        UpgradeToken usdc = new UpgradeToken();
        UpgradeToken eurc = new UpgradeToken();
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(usdc), address(eurc), address(this), address(this)));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        AndoMPPI escrow = AndoMPPI(address(proxy));

        usdc.mint(address(this), 1_000_000);
        usdc.approve(address(escrow), type(uint256).max);
        bytes32 channelId = escrow.openWithOperator(
            address(0xCAFE), address(0xBEEF), address(usdc), 1_000_000, bytes32(uint256(1)), address(0)
        );
        AndoMPPI.Channel memory beforeUpgrade = escrow.getChannel(channelId);

        ProxyAdmin admin = ProxyAdmin(address(uint160(uint256(vm.load(address(proxy), ERC1967_ADMIN_SLOT)))));
        AndoMPPIUpgradeV2 nextImplementation = new AndoMPPIUpgradeV2();
        vm.prank(address(0xBAD));
        (bool unauthorized,) = address(admin)
            .call(
                abi.encodeCall(
                    ProxyAdmin.upgradeAndCall,
                    (ITransparentUpgradeableProxy(address(proxy)), address(nextImplementation), bytes(""))
                )
            );
        require(!unauthorized, "non-owner upgraded proxy");

        admin.upgradeAndCall(ITransparentUpgradeableProxy(address(proxy)), address(nextImplementation), bytes(""));
        require(AndoMPPIUpgradeV2(address(proxy)).upgradeMarker() == 2, "proxy did not upgrade");
        AndoMPPI.Channel memory afterUpgrade = escrow.getChannel(channelId);
        require(afterUpgrade.payer == beforeUpgrade.payer, "payer storage changed");
        require(afterUpgrade.deposit == beforeUpgrade.deposit, "deposit storage changed");
        require(afterUpgrade.operator == beforeUpgrade.operator, "operator storage changed");
    }

    function testPopulatedStateRolesAndExitPathsSurviveUpgrade() public {
        UpgradeContext memory context = _createPopulatedContext();

        UpgradeSnapshot memory snapshot = UpgradeSnapshot({
            active: context.escrow.getChannel(context.active),
            closing: context.escrow.getChannel(context.closing),
            finalized: context.escrow.getChannel(context.finalized),
            domainSeparator: context.escrow.domainSeparator(),
            usdcEscrowBalance: context.usdc.balanceOf(address(context.escrow)),
            eurcEscrowBalance: context.eurc.balanceOf(address(context.escrow))
        });

        ProxyAdmin admin = ProxyAdmin(address(uint160(uint256(vm.load(address(context.proxy), ERC1967_ADMIN_SLOT)))));
        AndoMPPIUpgradeV2 nextImplementation = new AndoMPPIUpgradeV2();
        admin.upgradeAndCall(
            ITransparentUpgradeableProxy(address(context.proxy)), address(nextImplementation), bytes("")
        );

        AndoMPPIUpgradeV2 upgraded = AndoMPPIUpgradeV2(address(context.proxy));
        require(upgraded.upgradeMarker() == 2, "new implementation unavailable");
        require(upgraded.usdc() == address(context.usdc), "USDC configuration changed");
        require(upgraded.eurc() == address(context.eurc), "EURC configuration changed");
        require(upgraded.domainSeparator() == snapshot.domainSeparator, "EIP-712 domain changed");
        require(upgraded.paused(), "pause state changed");
        require(upgraded.hasRole(upgraded.DEFAULT_ADMIN_ROLE(), address(this)), "governance role changed");
        require(upgraded.hasRole(upgraded.PAUSER_ROLE(), address(this)), "guardian role changed");
        require(context.usdc.balanceOf(address(upgraded)) == snapshot.usdcEscrowBalance, "USDC escrow balance changed");
        require(context.eurc.balanceOf(address(upgraded)) == snapshot.eurcEscrowBalance, "EURC escrow balance changed");
        _assertChannelEqual(upgraded.getChannel(context.active), snapshot.active);
        _assertChannelEqual(upgraded.getChannel(context.closing), snapshot.closing);
        _assertChannelEqual(upgraded.getChannel(context.finalized), snapshot.finalized);

        bytes memory finalVoucher = _voucher(upgraded, context.active, 1_000_000);
        vm.prank(address(0xBEEF));
        upgraded.close(context.active, 1_000_000, finalVoucher);
        vm.warp(uint256(snapshot.closing.closeRequestedAt) + upgraded.CLOSE_GRACE_PERIOD());
        upgraded.withdraw(context.closing);
        require(upgraded.getChannel(context.active).finalized, "active channel could not exit after upgrade");
        require(upgraded.getChannel(context.closing).finalized, "closing channel could not exit after upgrade");
    }

    function testProductionUpgradeWaitsFortyEightHours() public {
        address[] memory proposers = new address[](1);
        proposers[0] = address(this);
        address[] memory executors = new address[](1);
        executors[0] = address(this);
        TimelockController timelock = new TimelockController(48 hours, proposers, executors, address(0));
        UpgradeToken usdc = new UpgradeToken();
        UpgradeToken eurc = new UpgradeToken();
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(usdc), address(eurc), address(timelock), address(this)));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(timelock), initialization);
        ProxyAdmin admin = ProxyAdmin(address(uint160(uint256(vm.load(address(proxy), ERC1967_ADMIN_SLOT)))));
        require(admin.owner() == address(timelock), "timelock does not own proxy admin");

        AndoMPPIUpgradeV2 nextImplementation = new AndoMPPIUpgradeV2();
        bytes memory upgrade = abi.encodeCall(
            ProxyAdmin.upgradeAndCall,
            (ITransparentUpgradeableProxy(address(proxy)), address(nextImplementation), bytes(""))
        );
        bytes32 salt = keccak256("AndoMPPI v2 audit-approved upgrade");
        timelock.schedule(address(admin), 0, upgrade, bytes32(0), salt, 48 hours);

        (bool executedEarly,) = address(timelock)
            .call(abi.encodeCall(TimelockController.execute, (address(admin), 0, upgrade, bytes32(0), salt)));
        require(!executedEarly, "upgrade bypassed timelock");

        vm.warp(block.timestamp + 48 hours);
        timelock.execute(address(admin), 0, upgrade, bytes32(0), salt);
        require(AndoMPPIUpgradeV2(address(proxy)).upgradeMarker() == 2, "delayed upgrade failed");
    }

    function _deploy(bytes memory creationCode) private returns (address deployed) {
        assembly ("memory-safe") {
            deployed := create(0, add(creationCode, 0x20), mload(creationCode))
        }
    }

    function _voucher(AndoMPPI escrow, bytes32 channelId, uint128 cumulativeAmount) private returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(SIGNER_KEY, escrow.voucherDigest(channelId, cumulativeAmount));
        return abi.encodePacked(r, s, v);
    }

    function _createPopulatedContext() private returns (UpgradeContext memory context) {
        context.usdc = new UpgradeToken();
        context.eurc = new UpgradeToken();
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization = abi.encodeCall(
            AndoMPPI.initialize, (address(context.usdc), address(context.eurc), address(this), address(this))
        );
        context.proxy = new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        context.escrow = AndoMPPI(address(context.proxy));

        context.usdc.mint(address(this), 10_000_000);
        context.eurc.mint(address(this), 10_000_000);
        context.usdc.approve(address(context.escrow), type(uint256).max);
        context.eurc.approve(address(context.escrow), type(uint256).max);

        context.active = context.escrow
            .openWithOperator(
                address(0xCAFE),
                address(0xBEEF),
                address(context.usdc),
                3_000_000,
                bytes32(uint256(101)),
                vm.addr(SIGNER_KEY)
            );
        context.escrow.topUp(context.active, 500_000);
        bytes memory activeVoucher = _voucher(context.escrow, context.active, 750_000);
        vm.prank(address(0xBEEF));
        context.escrow.settle(context.active, 750_000, activeVoucher);

        context.closing =
            context.escrow.open(address(0xCAFE), address(context.eurc), 2_000_000, bytes32(uint256(102)), address(0));
        context.escrow.requestClose(context.closing);

        context.finalized =
            context.escrow.open(address(0xCAFE), address(context.usdc), 1_000_000, bytes32(uint256(103)), address(0));
        vm.prank(address(0xCAFE));
        context.escrow.close(context.finalized, 0, "");
        context.escrow.pauseFunding();
    }

    function _assertChannelEqual(AndoMPPI.Channel memory actual, AndoMPPI.Channel memory expected) private pure {
        require(actual.payer == expected.payer, "payer storage changed");
        require(actual.payee == expected.payee, "payee storage changed");
        require(actual.operator == expected.operator, "operator storage changed");
        require(actual.token == expected.token, "token storage changed");
        require(actual.authorizedSigner == expected.authorizedSigner, "signer storage changed");
        require(actual.deposit == expected.deposit, "deposit storage changed");
        require(actual.settled == expected.settled, "settled storage changed");
        require(actual.closeRequestedAt == expected.closeRequestedAt, "close timer storage changed");
        require(actual.finalized == expected.finalized, "finalization storage changed");
    }
}
