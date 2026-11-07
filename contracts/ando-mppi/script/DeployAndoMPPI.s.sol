// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../src/AndoMPPI.sol";
import {IERC20Metadata} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {TimelockController} from "@openzeppelin/contracts/governance/TimelockController.sol";
import {ProxyAdmin} from "@openzeppelin/contracts/proxy/transparent/ProxyAdmin.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";
import {Script, console2} from "forge-std/Script.sol";

/// @title DeployAndoMPPI
/// @notice Foundry deployment script for the pre-audit AndoMPPI v1 Arc Testnet deployment.
/// @dev Uses an encrypted Foundry account or hardware wallet supplied to `forge script`.
/// No signing material is read from environment variables or written to deployment output.
contract DeployAndoMPPI is Script {
    uint256 public constant ARC_TESTNET_CHAIN_ID = 5_042_002;
    uint256 public constant UPGRADE_DELAY = 48 hours;
    address public constant ARC_TESTNET_USDC = 0x3600000000000000000000000000000000000000;
    address public constant ARC_TESTNET_EURC = 0x89B50855Aa3bE2F677cD6303Cec089B5F319D72a;

    bytes32 private constant ERC1967_ADMIN_SLOT = bytes32(uint256(keccak256("eip1967.proxy.admin")) - 1);
    bytes32 private constant USDC_SYMBOL = 0x5553444300000000000000000000000000000000000000000000000000000000;
    bytes32 private constant EURC_SYMBOL = 0x4555524300000000000000000000000000000000000000000000000000000000;
    bytes32 private constant USDC_SYMBOL_HASH = keccak256("USDC");
    bytes32 private constant EURC_SYMBOL_HASH = keccak256("EURC");

    struct Deployment {
        address implementation;
        address proxy;
        address proxyAdmin;
        address timelock;
        address governance;
        address guardian;
        address usdc;
        address eurc;
    }

    error CurrenciesMustDiffer();
    error GovernanceMustBeContract(address governance);
    error GuardianMatchesGovernance();
    error InvalidCurrencyMetadata(address token, bytes32 expectedSymbol, uint8 actualDecimals);
    error InvalidDeployer();
    error InvalidGuardian();
    error WrongChain(uint256 actualChainId);

    /// @notice Broadcasts the canonical Arc Testnet deployment configuration.
    /// @dev Required public environment values:
    /// `ANDO_MPPI_DEPLOYER`, `ANDO_MPPI_GOVERNANCE`, and `ANDO_MPPI_GUARDIAN`.
    function run() external returns (Deployment memory deployment) {
        address deployer = vm.envAddress("ANDO_MPPI_DEPLOYER");
        address governance = vm.envAddress("ANDO_MPPI_GOVERNANCE");
        address guardian = vm.envAddress("ANDO_MPPI_GUARDIAN");
        if (deployer == address(0)) revert InvalidDeployer();

        vm.startBroadcast(deployer);
        deployment = deploy(ARC_TESTNET_USDC, ARC_TESTNET_EURC, governance, guardian);
        vm.stopBroadcast();

        console2.log("AndoMPPI implementation", deployment.implementation);
        console2.log("AndoMPPI proxy", deployment.proxy);
        console2.log("ProxyAdmin", deployment.proxyAdmin);
        console2.log("TimelockController", deployment.timelock);
        console2.log("Governance proposer", deployment.governance);
        console2.log("Emergency guardian", deployment.guardian);
        console2.log("USDC", deployment.usdc);
        console2.log("EURC", deployment.eurc);
    }

    /// @notice Deploys and initializes the v1 implementation, timelock, proxy, and proxy admin.
    /// @dev Public for deterministic Foundry testing. The script contract is not itself deployed.
    function deploy(address usdc, address eurc, address governance, address guardian)
        public
        returns (Deployment memory deployment)
    {
        if (block.chainid != ARC_TESTNET_CHAIN_ID) revert WrongChain(block.chainid);
        if (governance.code.length == 0) revert GovernanceMustBeContract(governance);
        if (guardian == address(0)) revert InvalidGuardian();
        if (guardian == governance) revert GuardianMatchesGovernance();
        if (usdc == eurc) revert CurrenciesMustDiffer();
        _validateCurrency(usdc, USDC_SYMBOL, USDC_SYMBOL_HASH);
        _validateCurrency(eurc, EURC_SYMBOL, EURC_SYMBOL_HASH);

        address[] memory proposers = new address[](1);
        proposers[0] = governance;
        address[] memory executors = new address[](1);
        executors[0] = address(0);

        TimelockController timelock = new TimelockController(UPGRADE_DELAY, proposers, executors, address(0));
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization = abi.encodeCall(AndoMPPI.initialize, (usdc, eurc, address(timelock), guardian));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(timelock), initialization);
        address proxyAdmin = address(uint160(uint256(vm.load(address(proxy), ERC1967_ADMIN_SLOT))));

        deployment = Deployment({
            implementation: address(implementation),
            proxy: address(proxy),
            proxyAdmin: proxyAdmin,
            timelock: address(timelock),
            governance: governance,
            guardian: guardian,
            usdc: usdc,
            eurc: eurc
        });

        assert(ProxyAdmin(proxyAdmin).owner() == address(timelock));
        assert(AndoMPPI(address(proxy)).hasRole(AndoMPPI(address(proxy)).DEFAULT_ADMIN_ROLE(), address(timelock)));
        assert(AndoMPPI(address(proxy)).hasRole(AndoMPPI(address(proxy)).PAUSER_ROLE(), guardian));
    }

    function _validateCurrency(address token, bytes32 expectedSymbol, bytes32 expectedSymbolHash) private view {
        uint8 decimals = token.code.length == 0 ? 0 : IERC20Metadata(token).decimals();
        if (
            token.code.length == 0 || decimals != 6
                || keccak256(bytes(IERC20Metadata(token).symbol())) != expectedSymbolHash
        ) {
            revert InvalidCurrencyMetadata(token, expectedSymbol, decimals);
        }
    }
}
