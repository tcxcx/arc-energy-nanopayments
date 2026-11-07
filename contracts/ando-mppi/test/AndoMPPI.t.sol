// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AndoMPPI} from "../src/AndoMPPI.sol";
import {TransparentUpgradeableProxy} from "@openzeppelin/contracts/proxy/transparent/TransparentUpgradeableProxy.sol";

interface Vm {
    struct Log {
        bytes32[] topics;
        bytes data;
        address emitter;
    }

    function addr(uint256 privateKey) external returns (address);
    function getRecordedLogs() external returns (Log[] memory);
    function prank(address caller) external;
    function recordLogs() external;
    function sign(uint256 privateKey, bytes32 digest) external returns (uint8 v, bytes32 r, bytes32 s);
    function warp(uint256 timestamp) external;
}

contract TestToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

contract TestContractSigner {
    bytes4 private constant ERC1271_MAGIC_VALUE = 0x1626ba7e;
    bytes32 private expectedDigest;
    bytes32 private expectedSignatureHash;

    function allow(bytes32 digest, bytes calldata signature) external {
        expectedDigest = digest;
        expectedSignatureHash = keccak256(signature);
    }

    function isValidSignature(bytes32 digest, bytes calldata signature) external view returns (bytes4) {
        if (digest == expectedDigest && keccak256(signature) == expectedSignatureHash) {
            return ERC1271_MAGIC_VALUE;
        }
        return bytes4(0xffffffff);
    }
}

contract AndoMPPITest {
    // forge-lint: disable-next-line(screaming-snake-case-const)
    Vm private constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    // Public deterministic test fixtures. Never use or fund these keys outside tests.
    uint256 private constant PAYER_KEY = uint256(keccak256("AndoMPPI test payer"));
    uint256 private constant SIGNER_KEY = uint256(keccak256("AndoMPPI test signer"));
    uint256 private constant SECP256K1_ORDER = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;
    bytes32 private constant EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    address private payer;
    address private signer;
    address private constant PAYEE = address(0xCAFE);
    address private constant OPERATOR = address(0xBEEF);
    AndoMPPI private escrow;
    TestToken private token;
    TestToken private eurc;

    struct SettlementInput {
        bytes32 channelId;
        uint128 cumulativeAmount;
        bytes signature;
    }

    function setUp() public {
        payer = vm.addr(PAYER_KEY);
        signer = vm.addr(SIGNER_KEY);
        token = new TestToken();
        eurc = new TestToken();
        escrow = _newEscrow();
        token.mint(payer, 10_000_000);
        vm.prank(payer);
        token.approve(address(escrow), type(uint256).max);
    }

    function testContractIdentityAndVersion() public view {
        require(keccak256(bytes(escrow.CONTRACT_NAME())) == keccak256(bytes("AndoMPPI")), "wrong contract name");
        _assertEq(escrow.CONTRACT_VERSION(), 1);
        require(keccak256(bytes(escrow.NAME())) == keccak256(bytes("AndoMPPI")), "wrong signing domain");
        require(keccak256(bytes(escrow.VERSION())) == keccak256(bytes("1")), "wrong MPP domain version");
    }

    function testVoucherDomainIsBoundToAndoMppiV1AndThisArcDeployment() public view {
        bytes32 expectedDomainSeparator = keccak256(
            abi.encode(EIP712_DOMAIN_TYPEHASH, keccak256("AndoMPPI"), keccak256("1"), block.chainid, address(escrow))
        );

        require(escrow.domainSeparator() == expectedDomainSeparator, "unexpected voucher domain");
    }

    function testDirectOpenInterfaceAndChannelId() public {
        bytes32 salt = bytes32(uint256(101));
        bytes32 expectedChannelId =
            keccak256(abi.encode(payer, PAYEE, address(token), salt, signer, address(escrow), block.chainid));

        vm.prank(payer);
        (bool opened, bytes memory result) = address(escrow)
            .call(
                abi.encodeWithSignature(
                    "open(address,address,uint128,bytes32,address)",
                    PAYEE,
                    address(token),
                    uint128(1_000_000),
                    salt,
                    signer
                )
            );

        _assertTrue(opened);
        require(abi.decode(result, (bytes32)) == expectedChannelId, "unexpected direct channel id");
    }

    function testMandatoryLifecycleSettlesAndRefunds() public {
        bytes32 channelId = _open(bytes32(uint256(1)), signer, 5_000_000);
        bytes memory first = _voucher(SIGNER_KEY, channelId, 1_000_000);
        vm.prank(PAYEE);
        escrow.settle(channelId, 1_000_000, first);

        bytes memory finalVoucher = _voucher(SIGNER_KEY, channelId, 1_500_000);
        vm.prank(PAYEE);
        escrow.close(channelId, 1_500_000, finalVoucher);

        AndoMPPI.Channel memory channel = escrow.getChannel(channelId);
        _assertEq(channel.deposit, 5_000_000);
        _assertEq(channel.settled, 1_500_000);
        _assertTrue(channel.finalized);
        _assertEq(token.balanceOf(PAYEE), 1_500_000);
        _assertEq(token.balanceOf(payer), 8_500_000);
    }

    function testOperatorSettlesToPayeeWithoutReceivingFunds() public {
        bytes32 salt = bytes32(uint256(11));
        bytes32 channelId = escrow.computeChannelIdWithOperator(payer, PAYEE, OPERATOR, address(token), salt, signer);
        vm.prank(payer);
        escrow.openWithOperator(PAYEE, OPERATOR, address(token), 1_000_000, salt, signer);
        bytes memory voucher = _voucher(SIGNER_KEY, channelId, 400_000);

        vm.prank(OPERATOR);
        escrow.settle(channelId, 400_000, voucher);

        _assertEq(token.balanceOf(PAYEE), 400_000);
        _assertEq(token.balanceOf(OPERATOR), 0);
    }

    function testOperatorIsCryptographicallyBoundToChannelId() public {
        bytes32 salt = bytes32(uint256(102));
        bytes32 expectedChannelId =
            keccak256(abi.encode(payer, PAYEE, address(token), salt, signer, OPERATOR, address(escrow), block.chainid));
        (bool computed, bytes memory result) = address(escrow)
            .staticcall(
                abi.encodeWithSignature(
                    "computeChannelIdWithOperator(address,address,address,address,bytes32,address)",
                    payer,
                    PAYEE,
                    OPERATOR,
                    address(token),
                    salt,
                    signer
                )
            );

        _assertTrue(computed);
        require(abi.decode(result, (bytes32)) == expectedChannelId, "operator channel id mismatch");

        vm.prank(payer);
        bytes32 opened = escrow.openWithOperator(PAYEE, OPERATOR, address(token), 1_000_000, salt, signer);
        require(opened == expectedChannelId, "opened channel omitted operator");

        address otherOperator = address(0xF00D);
        (computed, result) = address(escrow)
            .staticcall(
                abi.encodeWithSignature(
                    "computeChannelIdWithOperator(address,address,address,address,bytes32,address)",
                    payer,
                    PAYEE,
                    otherOperator,
                    address(token),
                    salt,
                    signer
                )
            );
        _assertTrue(computed);
        require(abi.decode(result, (bytes32)) != expectedChannelId, "operator did not change channel id");
    }

    function testOperatorOpenRejectsZeroOperator() public {
        vm.prank(payer);
        (bool opened,) = address(escrow)
            .call(
                abi.encodeCall(
                    AndoMPPI.openWithOperator,
                    (PAYEE, address(0), address(token), uint128(1_000_000), bytes32(uint256(103)), signer)
                )
            );

        _assertTrue(!opened);
    }

    function testOperatorCanDiscoverAssignedChannelFromIndexedEvent() public {
        bytes32 salt = bytes32(uint256(108));
        bytes32 expectedChannelId =
            escrow.computeChannelIdWithOperator(payer, PAYEE, OPERATOR, address(token), salt, signer);
        bytes32 expectedSignature = keccak256("SettlementOperatorAssigned(bytes32,address)");

        vm.recordLogs();
        vm.prank(payer);
        escrow.openWithOperator(PAYEE, OPERATOR, address(token), 1_000_000, salt, signer);
        Vm.Log[] memory logs = vm.getRecordedLogs();

        bool found;
        for (uint256 index; index < logs.length; ++index) {
            if (logs[index].topics.length == 3 && logs[index].topics[0] == expectedSignature) {
                require(logs[index].topics[1] == expectedChannelId, "event channel is not indexed");
                require(address(uint160(uint256(logs[index].topics[2]))) == OPERATOR, "event operator is not indexed");
                found = true;
            }
        }
        _assertTrue(found);
    }

    function testOperatorClosesToPayeeAndRefundsPayer() public {
        bytes32 salt = bytes32(uint256(12));
        bytes32 channelId = escrow.computeChannelIdWithOperator(payer, PAYEE, OPERATOR, address(token), salt, signer);
        vm.prank(payer);
        escrow.openWithOperator(PAYEE, OPERATOR, address(token), 1_000_000, salt, signer);
        bytes memory voucher = _voucher(SIGNER_KEY, channelId, 250_000);

        vm.prank(OPERATOR);
        escrow.close(channelId, 250_000, voucher);

        _assertEq(token.balanceOf(PAYEE), 250_000);
        _assertEq(token.balanceOf(OPERATOR), 0);
        _assertEq(token.balanceOf(payer), 9_750_000);
    }

    function testForcedCloseCannotSkipGracePeriod() public {
        bytes32 channelId = _open(bytes32(uint256(2)), address(0), 5_000_000);
        vm.warp(1_000_000);
        vm.prank(payer);
        escrow.requestClose(channelId);

        vm.warp(1_000_000 + 15 minutes - 1);
        vm.prank(payer);
        (bool early,) = address(escrow).call(abi.encodeCall(AndoMPPI.withdraw, (channelId)));
        _assertTrue(!early);

        vm.warp(1_000_000 + 15 minutes);
        vm.prank(payer);
        escrow.withdraw(channelId);
        _assertEq(token.balanceOf(payer), 10_000_000);
    }

    function testRepeatedCloseRequestDoesNotRestartGracePeriod() public {
        bytes32 channelId = _open(bytes32(uint256(13)), address(0), 1_000_000);
        vm.warp(1_000_000);
        vm.prank(payer);
        escrow.requestClose(channelId);
        uint64 firstRequestedAt = escrow.getChannel(channelId).closeRequestedAt;

        vm.warp(1_000_100);
        vm.prank(payer);
        escrow.requestClose(channelId);

        _assertEq(escrow.getChannel(channelId).closeRequestedAt, firstRequestedAt);
    }

    function testFundingPauseBlocksNewChannels() public {
        escrow.pauseFunding();
        vm.prank(payer);
        (bool opened,) = address(escrow)
            .call(
                abi.encodeCall(
                    AndoMPPI.open, (PAYEE, address(token), uint128(1_000_000), bytes32(uint256(14)), address(0))
                )
            );
        _assertTrue(!opened);
    }

    function testFundingPauseBlocksTopUps() public {
        bytes32 channelId = _open(bytes32(uint256(15)), address(0), 1_000_000);
        escrow.pauseFunding();
        vm.prank(payer);
        (bool toppedUp,) = address(escrow).call(abi.encodeCall(AndoMPPI.topUp, (channelId, uint128(1))));
        _assertTrue(!toppedUp);
    }

    function testGovernanceCanUnpauseFunding() public {
        escrow.pauseFunding();
        escrow.unpauseFunding();

        bytes32 channelId = _open(bytes32(uint256(25)), address(0), 1_000_000);

        _assertTrue(escrow.getChannel(channelId).payer == payer);
    }

    function testTopUpCancelsPendingClose() public {
        bytes32 channelId = _open(bytes32(uint256(3)), signer, 1_000_000);
        vm.prank(payer);
        escrow.requestClose(channelId);
        vm.prank(payer);
        escrow.topUp(channelId, 500_000);

        AndoMPPI.Channel memory channel = escrow.getChannel(channelId);
        _assertEq(channel.deposit, 1_500_000);
        _assertEq(channel.closeRequestedAt, 0);
    }

    function testZeroTopUpRevertsAndCannotCancelClose() public {
        bytes32 channelId = _open(bytes32(uint256(16)), signer, 1_000_000);
        vm.prank(payer);
        escrow.requestClose(channelId);
        uint64 requestedAt = escrow.getChannel(channelId).closeRequestedAt;

        vm.prank(payer);
        (bool toppedUp,) = address(escrow).call(abi.encodeCall(AndoMPPI.topUp, (channelId, uint128(0))));

        _assertTrue(!toppedUp);
        _assertEq(escrow.getChannel(channelId).closeRequestedAt, requestedAt);
    }

    function testInvalidOpenInputsRevert() public {
        vm.prank(payer);
        (bool zeroPayee,) = address(escrow)
            .call(
                abi.encodeCall(
                    AndoMPPI.open, (address(0), address(token), uint128(1_000_000), bytes32(uint256(26)), address(0))
                )
            );
        _assertTrue(!zeroPayee);

        vm.prank(payer);
        (bool zeroDeposit,) = address(escrow)
            .call(abi.encodeCall(AndoMPPI.open, (PAYEE, address(token), uint128(0), bytes32(uint256(27)), address(0))));
        _assertTrue(!zeroDeposit);
    }

    function testTopUpCannotOverflowUint128Deposit() public {
        token.mint(payer, type(uint128).max);
        bytes32 channelId = _open(bytes32(uint256(28)), address(0), type(uint128).max);

        vm.prank(payer);
        (bool toppedUp,) = address(escrow).call(abi.encodeCall(AndoMPPI.topUp, (channelId, uint128(1))));

        _assertTrue(!toppedUp);
        _assertEq(escrow.getChannel(channelId).deposit, type(uint128).max);
    }

    function testSettlementRejectsInvalidAmountsAndSignatures() public {
        bytes32 channelId = _open(bytes32(uint256(29)), signer, 1_000_000);
        bytes memory wrongSignature = _voucher(PAYER_KEY, channelId, 100_000);
        vm.prank(PAYEE);
        (bool wrongSigner,) =
            address(escrow).call(abi.encodeCall(AndoMPPI.settle, (channelId, uint128(100_000), wrongSignature)));
        _assertTrue(!wrongSigner);

        bytes memory excessiveVoucher = _voucher(SIGNER_KEY, channelId, 1_000_001);
        vm.prank(PAYEE);
        (bool excessive,) =
            address(escrow).call(abi.encodeCall(AndoMPPI.settle, (channelId, uint128(1_000_001), excessiveVoucher)));
        _assertTrue(!excessive);

        bytes memory validVoucher = _voucher(SIGNER_KEY, channelId, 100_000);
        vm.prank(PAYEE);
        escrow.settle(channelId, 100_000, validVoucher);
        vm.prank(PAYEE);
        (bool repeated,) =
            address(escrow).call(abi.encodeCall(AndoMPPI.settle, (channelId, uint128(100_000), validVoucher)));
        _assertTrue(!repeated);
    }

    function testMissingAndFinalizedChannelsRejectOperations() public {
        bytes32 missing = bytes32(uint256(30));
        (bool readMissing,) = address(escrow).call(abi.encodeCall(AndoMPPI.getChannel, (missing)));
        _assertTrue(!readMissing);
        vm.prank(PAYEE);
        (bool settleMissing,) = address(escrow).call(abi.encodeCall(AndoMPPI.settle, (missing, uint128(1), bytes(""))));
        _assertTrue(!settleMissing);

        bytes32 finalized = _open(bytes32(uint256(31)), address(0), 1_000_000);
        vm.prank(PAYEE);
        escrow.close(finalized, 0, "");
        vm.prank(payer);
        (bool topUpFinalized,) = address(escrow).call(abi.encodeCall(AndoMPPI.topUp, (finalized, uint128(1))));
        _assertTrue(!topUpFinalized);
    }

    function testOnlyPayerCanFundOrBeginForcedClose() public {
        bytes32 channelId = _open(bytes32(uint256(32)), address(0), 1_000_000);
        address unauthorized = address(0xBAD);

        vm.prank(unauthorized);
        (bool toppedUp,) = address(escrow).call(abi.encodeCall(AndoMPPI.topUp, (channelId, uint128(1))));
        _assertTrue(!toppedUp);
        vm.prank(unauthorized);
        (bool requested,) = address(escrow).call(abi.encodeCall(AndoMPPI.requestClose, (channelId)));
        _assertTrue(!requested);
        vm.prank(unauthorized);
        (bool withdrew,) = address(escrow).call(abi.encodeCall(AndoMPPI.withdraw, (channelId)));
        _assertTrue(!withdrew);
    }

    function testWithdrawRequiresCloseRequest() public {
        bytes32 channelId = _open(bytes32(uint256(33)), address(0), 1_000_000);

        vm.prank(payer);
        (bool withdrew,) = address(escrow).call(abi.encodeCall(AndoMPPI.withdraw, (channelId)));

        _assertTrue(!withdrew);
    }

    function testCloseRejectsAmountAboveDeposit() public {
        bytes32 channelId = _open(bytes32(uint256(34)), signer, 1_000_000);

        vm.prank(PAYEE);
        (bool closed,) =
            address(escrow).call(abi.encodeCall(AndoMPPI.close, (channelId, uint128(1_000_001), bytes(""))));

        _assertTrue(!closed);
    }

    function testCloseRequestAtTimestampZeroStillStartsGracePeriod() public {
        bytes32 channelId = _open(bytes32(uint256(35)), address(0), 1_000_000);
        vm.warp(0);

        vm.prank(payer);
        escrow.requestClose(channelId);

        _assertEq(escrow.getChannel(channelId).closeRequestedAt, 1);
    }

    function testFinalizedChannelIdCannotBeReopened() public {
        bytes32 salt = bytes32(uint256(4));
        bytes32 channelId = _open(salt, address(0), 1_000_000);
        vm.prank(PAYEE);
        escrow.close(channelId, 0, "");

        vm.prank(payer);
        (bool reopened,) = address(escrow)
            .call(abi.encodeCall(AndoMPPI.open, (PAYEE, address(token), uint128(1_000_000), salt, address(0))));
        _assertTrue(!reopened);
    }

    function testVoucherIsBoundToContractDomain() public {
        bytes32 channelId = _open(bytes32(uint256(5)), signer, 1_000_000);
        AndoMPPI other = _newEscrow();
        bytes32 wrongDigest = other.voucherDigest(channelId, 100);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(SIGNER_KEY, wrongDigest);
        bytes memory signature = abi.encodePacked(r, s, v);

        vm.prank(PAYEE);
        (bool accepted,) = address(escrow).call(abi.encodeCall(AndoMPPI.settle, (channelId, uint128(100), signature)));
        _assertTrue(!accepted);
    }

    function testDefaultPayerSignerCanAuthorizeSettlement() public {
        bytes32 channelId = _open(bytes32(uint256(17)), address(0), 1_000_000);
        bytes memory voucher = _voucher(PAYER_KEY, channelId, 300_000);

        vm.prank(PAYEE);
        escrow.settle(channelId, 300_000, voucher);

        _assertEq(token.balanceOf(PAYEE), 300_000);
    }

    function testERC1271ContractSignerCanAuthorizeSettlement() public {
        TestContractSigner contractSigner = new TestContractSigner();
        bytes memory signature = bytes("contract-wallet-approval");
        bytes32 channelId = _open(bytes32(uint256(18)), address(contractSigner), 1_000_000);
        contractSigner.allow(escrow.voucherDigest(channelId, 350_000), signature);

        vm.prank(PAYEE);
        escrow.settle(channelId, 350_000, signature);

        _assertEq(token.balanceOf(PAYEE), 350_000);
    }

    function testHighSSignatureIsRejected() public {
        bytes32 channelId = _open(bytes32(uint256(19)), signer, 1_000_000);
        bytes32 digest = escrow.voucherDigest(channelId, 100_000);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(SIGNER_KEY, digest);
        bytes32 highS = bytes32(SECP256K1_ORDER - uint256(s));
        uint8 highV = v == 27 ? 28 : 27;

        vm.prank(PAYEE);
        (bool settled,) = address(escrow)
            .call(abi.encodeCall(AndoMPPI.settle, (channelId, uint128(100_000), abi.encodePacked(r, highS, highV))));
        _assertTrue(!settled);
        _assertEq(escrow.getChannel(channelId).settled, 0);
    }

    function testUnauthorizedAddressCannotSettleOrClose() public {
        bytes32 channelId = _open(bytes32(uint256(20)), signer, 1_000_000);
        bytes memory voucher = _voucher(SIGNER_KEY, channelId, 100_000);
        address unauthorized = address(0xBAD);

        vm.prank(unauthorized);
        (bool settled,) = address(escrow).call(abi.encodeCall(AndoMPPI.settle, (channelId, uint128(100_000), voucher)));
        _assertTrue(!settled);

        vm.prank(unauthorized);
        (bool closed,) = address(escrow).call(abi.encodeCall(AndoMPPI.close, (channelId, uint128(100_000), voucher)));
        _assertTrue(!closed);
    }

    function testPauseNeverBlocksSettlementOrExit() public {
        bytes32 cooperativeChannel = _open(bytes32(uint256(21)), signer, 1_000_000);
        bytes memory voucher = _voucher(SIGNER_KEY, cooperativeChannel, 250_000);
        bytes32 forcedChannel = _open(bytes32(uint256(22)), signer, 1_000_000);

        escrow.pauseFunding();
        vm.prank(PAYEE);
        escrow.settle(cooperativeChannel, 250_000, voucher);
        vm.prank(PAYEE);
        escrow.close(cooperativeChannel, 250_000, "");

        vm.prank(payer);
        escrow.requestClose(forcedChannel);
        vm.warp(block.timestamp + escrow.CLOSE_GRACE_PERIOD());
        vm.prank(payer);
        escrow.withdraw(forcedChannel);

        _assertTrue(escrow.getChannel(cooperativeChannel).finalized);
        _assertTrue(escrow.getChannel(forcedChannel).finalized);
    }

    function testBatchQueryReturnsPublicChannelState() public {
        bytes32 first = _open(bytes32(uint256(23)), signer, 1_000_000);
        bytes32 second = _open(bytes32(uint256(24)), address(0), 2_000_000);
        bytes32[] memory channelIds = new bytes32[](3);
        channelIds[0] = first;
        channelIds[1] = second;
        channelIds[2] = bytes32(uint256(999));

        AndoMPPI.Channel[] memory channels = escrow.getChannelsBatch(channelIds);

        _assertEq(channels.length, 3);
        require(channels[0].payer == payer, "first channel unavailable");
        _assertEq(channels[1].deposit, 2_000_000);
        require(channels[2].payer == address(0), "missing channel was not empty");
        require(escrow.channels(first).payer == payer, "mapping-compatible channel read failed");
        require(escrow.getVoucherDigest(first, 10) == escrow.voucherDigest(first, 10), "digest aliases differ");
    }

    function testBatchQueryRejectsUnboundedReads() public {
        bytes32[] memory channelIds = new bytes32[](escrow.MAX_BATCH_SIZE() + 1);

        (bool queried,) = address(escrow).call(abi.encodeCall(AndoMPPI.getChannelsBatch, (channelIds)));

        _assertTrue(!queried);
    }

    function testBatchSettlementPaysMultipleChannelsCumulatively() public {
        bytes32 first = _open(bytes32(uint256(104)), signer, 2_000_000);
        bytes32 second = _open(bytes32(uint256(105)), signer, 3_000_000);
        SettlementInput[] memory settlements = new SettlementInput[](2);
        settlements[0] = SettlementInput({
            channelId: first, cumulativeAmount: 600_000, signature: _voucher(SIGNER_KEY, first, 600_000)
        });
        settlements[1] = SettlementInput({
            channelId: second, cumulativeAmount: 900_000, signature: _voucher(SIGNER_KEY, second, 900_000)
        });

        vm.prank(PAYEE);
        (bool settled,) =
            address(escrow).call(abi.encodeWithSignature("settleBatch((bytes32,uint128,bytes)[])", settlements));

        _assertTrue(settled);
        _assertEq(escrow.getChannel(first).settled, 600_000);
        _assertEq(escrow.getChannel(second).settled, 900_000);
        _assertEq(token.balanceOf(PAYEE), 1_500_000);
    }

    function testBatchSettlementRevertsAtomicallyWhenOneVoucherIsInvalid() public {
        bytes32 first = _open(bytes32(uint256(106)), signer, 2_000_000);
        bytes32 second = _open(bytes32(uint256(107)), signer, 3_000_000);
        SettlementInput[] memory settlements = new SettlementInput[](2);
        settlements[0] = SettlementInput({
            channelId: first, cumulativeAmount: 600_000, signature: _voucher(SIGNER_KEY, first, 600_000)
        });
        settlements[1] = SettlementInput({
            channelId: second, cumulativeAmount: 900_000, signature: _voucher(PAYER_KEY, second, 900_000)
        });

        vm.prank(PAYEE);
        (bool settled, bytes memory reason) =
            address(escrow).call(abi.encodeWithSignature("settleBatch((bytes32,uint128,bytes)[])", settlements));

        _assertTrue(!settled);
        require(_selector(reason) == AndoMPPI.InvalidSignature.selector, "unexpected batch failure");
        _assertEq(escrow.getChannel(first).settled, 0);
        _assertEq(escrow.getChannel(second).settled, 0);
        _assertEq(token.balanceOf(PAYEE), 0);
    }

    function testBatchSettlementRejectsUnboundedWork() public {
        SettlementInput[] memory settlements = new SettlementInput[](escrow.MAX_BATCH_SIZE() + 1);

        vm.prank(PAYEE);
        (bool settled, bytes memory reason) =
            address(escrow).call(abi.encodeWithSignature("settleBatch((bytes32,uint128,bytes)[])", settlements));

        _assertTrue(!settled);
        require(_selector(reason) == AndoMPPI.BatchTooLarge.selector, "missing batch limit");
    }

    function testDerivedChannelStateTracksOpenClosingAndFinalizedLifecycle() public {
        bytes32 channelId = _open(bytes32(uint256(109)), signer, 2_000_000);

        (uint8 status, uint128 remaining, uint256 closeGraceEndsAt) = _channelState(channelId);
        _assertEq(status, 1);
        _assertEq(remaining, 2_000_000);
        _assertEq(closeGraceEndsAt, 0);

        vm.warp(1_000_000);
        vm.prank(payer);
        escrow.requestClose(channelId);

        (status, remaining, closeGraceEndsAt) = _channelState(channelId);
        _assertEq(status, 2);
        _assertEq(remaining, 2_000_000);
        _assertEq(closeGraceEndsAt, 1_000_000 + 15 minutes);

        vm.prank(PAYEE);
        escrow.close(channelId, 0, "");

        (status, remaining, closeGraceEndsAt) = _channelState(channelId);
        _assertEq(status, 3);
        _assertEq(remaining, 0);
        _assertEq(closeGraceEndsAt, 0);
    }

    function testDerivedChannelStateReportsUnknownChannelWithoutReverting() public view {
        (uint8 status, uint128 remaining, uint256 closeGraceEndsAt) = _channelState(bytes32(uint256(999_999)));

        _assertEq(status, 0);
        _assertEq(remaining, 0);
        _assertEq(closeGraceEndsAt, 0);
    }

    function testCurrencyAndDomainViewsExposeOnlyPublicConfiguration() public view {
        require(escrow.usdc() == address(token), "wrong USDC");
        require(escrow.eurc() == address(eurc), "wrong EURC");
        _assertTrue(escrow.isAcceptedToken(address(token)));
        _assertTrue(escrow.isAcceptedToken(address(eurc)));
        _assertTrue(!escrow.isAcceptedToken(address(0xBAD)));
        _assertEq(escrow.closeGracePeriod(), escrow.CLOSE_GRACE_PERIOD());
        require(escrow.domainSeparator() != bytes32(0), "missing domain separator");
    }

    function testFuzzCumulativeSettlementPreservesEscrowAccounting(uint128 depositSeed, uint128 amountSeed) public {
        uint128 deposit = uint128(uint256(depositSeed) % 5_000_000) + 1;
        uint128 cumulativeAmount = uint128(uint256(amountSeed) % deposit) + 1;
        bytes32 salt = keccak256(abi.encode(depositSeed, amountSeed));
        bytes32 channelId = _open(salt, signer, deposit);
        bytes memory voucher = _voucher(SIGNER_KEY, channelId, cumulativeAmount);

        vm.prank(PAYEE);
        escrow.settle(channelId, cumulativeAmount, voucher);

        _assertEq(token.balanceOf(address(escrow)), deposit - cumulativeAmount);
        _assertEq(token.balanceOf(PAYEE), cumulativeAmount);
        _assertEq(escrow.getChannel(channelId).settled, cumulativeAmount);
    }

    function _open(bytes32 salt, address authorizedSigner, uint128 deposit) private returns (bytes32 channelId) {
        channelId = escrow.computeChannelId(payer, PAYEE, address(token), salt, authorizedSigner);
        vm.prank(payer);
        escrow.open(PAYEE, address(token), deposit, salt, authorizedSigner);
    }

    function _newEscrow() private returns (AndoMPPI instance) {
        AndoMPPI implementation = new AndoMPPI();
        bytes memory initialization =
            abi.encodeCall(AndoMPPI.initialize, (address(token), address(eurc), address(this), address(this)));
        TransparentUpgradeableProxy proxy =
            new TransparentUpgradeableProxy(address(implementation), address(this), initialization);
        instance = AndoMPPI(address(proxy));
    }

    function _voucher(uint256 key, bytes32 channelId, uint128 cumulativeAmount) private returns (bytes memory) {
        bytes32 digest = escrow.voucherDigest(channelId, cumulativeAmount);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }

    function _assertEq(uint256 actual, uint256 expected) private pure {
        require(actual == expected, "not equal");
    }

    function _assertTrue(bool value) private pure {
        require(value, "not true");
    }

    function _selector(bytes memory revertData) private pure returns (bytes4 result) {
        if (revertData.length < 4) return bytes4(0);
        assembly ("memory-safe") {
            result := mload(add(revertData, 0x20))
        }
    }

    function _channelState(bytes32 channelId)
        private
        view
        returns (uint8 status, uint128 remaining, uint256 closeGraceEndsAt)
    {
        (bool read, bytes memory result) =
            address(escrow).staticcall(abi.encodeWithSignature("getChannelState(bytes32)", channelId));
        require(read, "channel state unavailable");
        return abi.decode(result, (uint8, uint128, uint256));
    }
}
