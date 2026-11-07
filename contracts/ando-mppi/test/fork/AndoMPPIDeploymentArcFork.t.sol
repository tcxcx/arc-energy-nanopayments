// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../../src/AndoMPPI.sol";
import {DeployAndoMPPI} from "../../script/DeployAndoMPPI.s.sol";
import {ProxyAdmin} from "@openzeppelin/contracts/proxy/transparent/ProxyAdmin.sol";
import {TimelockController} from "@openzeppelin/contracts/governance/TimelockController.sol";
import {Test} from "forge-std/Test.sol";

contract ArcForkDeploymentGovernance {}

/// @notice Full deployment simulation using the official Arc Testnet currency contracts.
/// @dev Runs on a local fork only and never broadcasts a transaction.
contract AndoMPPIDeploymentArcForkTest is Test {
    uint256 private constant ARC_TESTNET_CHAIN_ID = 5_042_002;
    uint256 private constant ARC_TESTNET_FORK_BLOCK = 54_275_166;
    address private constant ARC_TESTNET_USDC = 0x3600000000000000000000000000000000000000;
    address private constant ARC_TESTNET_EURC = 0x89B50855Aa3bE2F677cD6303Cec089B5F319D72a;

    DeployAndoMPPI private deploymentScript;
    ArcForkDeploymentGovernance private governance;
    address private deployer;
    address private guardian;

    function setUp() public {
        if (!vm.envOr("RUN_ARC_FORK", false)) {
            vm.skip(true);
            return;
        }

        string memory rpcUrl = vm.envOr("ARC_TESTNET_RPC_URL", string("https://rpc.testnet.arc.io"));
        vm.createSelectFork(rpcUrl, ARC_TESTNET_FORK_BLOCK);
        deploymentScript = new DeployAndoMPPI();
        governance = new ArcForkDeploymentGovernance();
        deployer = makeAddr("AndoMPPI Arc fork deployer");
        guardian = makeAddr("AndoMPPI Arc fork guardian");
        // forge-lint: disable-next-line(unsafe-cheatcode)
        vm.setEnv("ANDO_MPPI_DEPLOYER", vm.toString(deployer));
        // forge-lint: disable-next-line(unsafe-cheatcode)
        vm.setEnv("ANDO_MPPI_GOVERNANCE", vm.toString(address(governance)));
        // forge-lint: disable-next-line(unsafe-cheatcode)
        vm.setEnv("ANDO_MPPI_GUARDIAN", vm.toString(guardian));
    }

    function testDeploymentAgainstOfficialArcConfiguration() public {
        DeployAndoMPPI.Deployment memory deployment = deploymentScript.run();

        AndoMPPI escrow = AndoMPPI(deployment.proxy);
        TimelockController timelock = TimelockController(payable(deployment.timelock));

        assertEq(block.chainid, ARC_TESTNET_CHAIN_ID, "unexpected Arc chain");
        assertEq(deployment.governance, address(governance), "wrong governance");
        assertEq(deployment.guardian, guardian, "wrong guardian");
        assertEq(escrow.usdc(), ARC_TESTNET_USDC, "wrong official USDC");
        assertEq(escrow.eurc(), ARC_TESTNET_EURC, "wrong official EURC");
        assertEq(timelock.getMinDelay(), 48 hours, "wrong timelock delay");
        assertTrue(timelock.hasRole(timelock.PROPOSER_ROLE(), address(governance)), "missing proposer");
        assertTrue(timelock.hasRole(timelock.CANCELLER_ROLE(), address(governance)), "missing canceller");
        assertTrue(timelock.hasRole(timelock.EXECUTOR_ROLE(), address(0)), "execution is not open");
        assertEq(ProxyAdmin(deployment.proxyAdmin).owner(), deployment.timelock, "wrong proxy admin owner");
        assertTrue(escrow.hasRole(escrow.DEFAULT_ADMIN_ROLE(), deployment.timelock), "wrong contract admin");
        assertTrue(escrow.hasRole(escrow.PAUSER_ROLE(), guardian), "wrong guardian");
    }
}
