# Arc Testnet deployment procedure

This procedure deploys the pre-audit AndoMPPI v1 implementation, a 48-hour
`TimelockController`, a `TransparentUpgradeableProxy`, and its dedicated
`ProxyAdmin`.

The deployer receives no contract role. The timelock owns the proxy admin and
the AndoMPPI admin role. A deployed governance contract is the sole proposer
and canceller, execution is open after the delay, and a separate guardian can
pause only new funding.

## Signing

Use an encrypted Foundry keystore for testnet or a hardware wallet for any
future production deployment. Never pass a private key on the command line,
store one in an environment variable, or commit signing material.

The deployment script reads only public addresses:

```sh
export ARC_TESTNET_RPC_URL=https://rpc.testnet.arc.io
export ANDO_MPPI_DEPLOYER=0x...
export ANDO_MPPI_GOVERNANCE=0x...
export ANDO_MPPI_GUARDIAN=0x...
```

`ANDO_MPPI_GOVERNANCE` must already contain contract code.
`ANDO_MPPI_GUARDIAN` must be a different non-zero address.

## Preflight

Confirm the public configuration and run the complete local matrix:

```sh
cast chain-id --rpc-url "$ARC_TESTNET_RPC_URL"
cast code "$ANDO_MPPI_GOVERNANCE" --rpc-url "$ARC_TESTNET_RPC_URL"
cast balance "$ANDO_MPPI_DEPLOYER" --rpc-url "$ARC_TESTNET_RPC_URL"
FOUNDRY_PROFILE=matrix forge test -vv
```

Simulate the exact deployment without broadcasting:

```sh
forge script script/DeployAndoMPPI.s.sol:DeployAndoMPPI \
  --rpc-url "$ARC_TESTNET_RPC_URL" \
  --sender "$ANDO_MPPI_DEPLOYER" \
  -vvv
```

Review every predicted address and transaction before continuing.

## Broadcast and verification

Broadcast from an encrypted keystore account:

```sh
forge script script/DeployAndoMPPI.s.sol:DeployAndoMPPI \
  --rpc-url "$ARC_TESTNET_RPC_URL" \
  --account <FOUNDRY_ACCOUNT> \
  --sender "$ANDO_MPPI_DEPLOYER" \
  --broadcast \
  --verify \
  --verifier blockscout \
  --verifier-url https://testnet.arcscan.app/api/ \
  -vvv
```

Foundry writes resumable transaction data under ignored `broadcast/` and
`cache/` directories. If broadcasting succeeds but verification needs to be
retried, verify the implementation, timelock, proxy, and proxy admin separately
with `forge verify-contract` using the same compiler settings and constructor
arguments recorded in the broadcast artifact.

## Required public record

After confirmation and source verification, add a deployment manifest under
`../../deployments/arc-testnet.json` containing:

- chain ID and deployment block;
- proxy, implementation, proxy admin, and timelock addresses;
- governance, guardian, USDC, and EURC addresses;
- deployment transaction hashes and source commit;
- compiler and optimizer settings;
- explorer links and verification status; and
- the implementation history.

Never record a key, keystore, password, credential, internal endpoint, or
unpublished wallet label.
