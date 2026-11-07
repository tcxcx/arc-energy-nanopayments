// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../src/AndoMPPI.sol";
import {DeployAndoMPPI} from "../script/DeployAndoMPPI.s.sol";
import {ProxyAdmin} from "@openzeppelin/contracts/proxy/transparent/ProxyAdmin.sol";
import {TimelockController} from "@openzeppelin/contracts/governance/TimelockController.sol";
import {Test} from "forge-std/Test.sol";

contract DeploymentToken {
    string public symbol;
    uint8 public decimals;

    constructor(string memory symbol_, uint8 decimals_) {
        symbol = symbol_;
        decimals = decimals_;
    }
}

contract DeploymentGovernance {}

contract AndoMPPIDeploymentTest is Test {
    uint256 private constant ARC_TESTNET_CHAIN_ID = 5_042_002;
    uint256 private constant UPGRADE_DELAY = 48 hours;

    DeployAndoMPPI private deploymentScript;
    DeploymentToken private usdc;
    DeploymentToken private eurc;
    DeploymentGovernance private governance;
    address private guardian;

    function setUp() public {
        vm.chainId(ARC_TESTNET_CHAIN_ID);
        deploymentScript = new DeployAndoMPPI();
        usdc = new DeploymentToken("USDC", 6);
        eurc = new DeploymentToken("EURC", 6);
        governance = new DeploymentGovernance();
        guardian = makeAddr("AndoMPPI deployment guardian");
    }

    function testDeploymentLeavesAuthorityOnlyWithTimelockGovernanceAndGuardian() public {
        DeployAndoMPPI.Deployment memory deployment =
            deploymentScript.deploy(address(usdc), address(eurc), address(governance), guardian);

        AndoMPPI escrow = AndoMPPI(deployment.proxy);
        TimelockController timelock = TimelockController(payable(deployment.timelock));
        ProxyAdmin proxyAdmin = ProxyAdmin(deployment.proxyAdmin);

        assertGt(deployment.implementation.code.length, 0, "implementation missing");
        assertGt(deployment.proxy.code.length, 0, "proxy missing");
        assertGt(deployment.proxyAdmin.code.length, 0, "proxy admin missing");
        assertGt(deployment.timelock.code.length, 0, "timelock missing");
        assertEq(escrow.usdc(), address(usdc), "wrong USDC");
        assertEq(escrow.eurc(), address(eurc), "wrong EURC");
        assertEq(escrow.CONTRACT_VERSION(), 1, "wrong contract version");
        assertEq(escrow.CLOSE_GRACE_PERIOD(), 15 minutes, "wrong close grace");

        assertEq(timelock.getMinDelay(), UPGRADE_DELAY, "wrong upgrade delay");
        assertTrue(timelock.hasRole(timelock.DEFAULT_ADMIN_ROLE(), address(timelock)), "timelock not self-admin");
        assertFalse(timelock.hasRole(timelock.DEFAULT_ADMIN_ROLE(), address(deploymentScript)), "deployer is admin");
        assertTrue(timelock.hasRole(timelock.PROPOSER_ROLE(), address(governance)), "governance not proposer");
        assertTrue(timelock.hasRole(timelock.CANCELLER_ROLE(), address(governance)), "governance not canceller");
        assertTrue(timelock.hasRole(timelock.EXECUTOR_ROLE(), address(0)), "execution is not open");

        assertEq(proxyAdmin.owner(), address(timelock), "timelock does not own proxy admin");
        assertTrue(escrow.hasRole(escrow.DEFAULT_ADMIN_ROLE(), address(timelock)), "timelock not contract admin");
        assertTrue(escrow.hasRole(escrow.PAUSER_ROLE(), guardian), "guardian not pauser");
        assertFalse(
            escrow.hasRole(escrow.DEFAULT_ADMIN_ROLE(), address(deploymentScript)), "deployer controls contract"
        );
        assertFalse(escrow.hasRole(escrow.PAUSER_ROLE(), address(deploymentScript)), "deployer can pause");

        bytes memory initializeCall =
            abi.encodeCall(AndoMPPI.initialize, (address(usdc), address(eurc), address(timelock), guardian));
        (bool implementationInitialized,) = deployment.implementation.call(initializeCall);
        assertFalse(implementationInitialized, "implementation is not initialization-locked");
        (bool proxyInitializedTwice,) = deployment.proxy.call(initializeCall);
        assertFalse(proxyInitializedTwice, "proxy initialized twice");
    }

    function testDeploymentRejectsWrongChain() public {
        vm.chainId(1);
        vm.expectRevert(abi.encodeWithSelector(DeployAndoMPPI.WrongChain.selector, 1));
        deploymentScript.deploy(address(usdc), address(eurc), address(governance), guardian);
    }

    function testDeploymentRequiresContractGovernanceAndSeparateGuardian() public {
        vm.expectRevert(abi.encodeWithSelector(DeployAndoMPPI.GovernanceMustBeContract.selector, address(0xA11CE)));
        deploymentScript.deploy(address(usdc), address(eurc), address(0xA11CE), guardian);

        vm.expectRevert(DeployAndoMPPI.GuardianMatchesGovernance.selector);
        deploymentScript.deploy(address(usdc), address(eurc), address(governance), address(governance));
    }

    function testDeploymentRejectsIncorrectCurrencyMetadata() public {
        DeploymentToken wrongDecimals = new DeploymentToken("USDC", 18);
        vm.expectRevert(
            abi.encodeWithSelector(
                DeployAndoMPPI.InvalidCurrencyMetadata.selector,
                address(wrongDecimals),
                0x5553444300000000000000000000000000000000000000000000000000000000,
                uint8(18)
            )
        );
        deploymentScript.deploy(address(wrongDecimals), address(eurc), address(governance), guardian);
    }
}
