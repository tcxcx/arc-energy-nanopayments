// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

/// @title IAndoMPPI
/// @notice Public Arc interface for MPP-I cumulative payment channels.
/// @dev Implementations must never accept or store HTTP bearer credentials,
/// session secrets, prompts, responses, API keys, or user application metadata.
interface IAndoMPPI {
    struct Channel {
        address payer;
        address payee;
        address operator;
        address token;
        address authorizedSigner;
        uint128 deposit;
        uint128 settled;
        uint64 closeRequestedAt;
        bool finalized;
    }

    struct Settlement {
        bytes32 channelId;
        uint128 cumulativeAmount;
        bytes signature;
    }

    enum ChannelStatus {
        None,
        Open,
        Closing,
        Finalized
    }

    error AmountExceedsDeposit();
    error AmountNotIncreasing();
    error BatchTooLarge();
    error ChannelAlreadyExists();
    error ChannelFinalized();
    error ChannelNotFound();
    error CloseNotReady();
    error DepositOverflow();
    error InvalidCurrency();
    error InvalidSignature();
    error NotPayeeOrOperator();
    error NotPayer();
    error TokenTransferMismatch();
    error ZeroAddress();
    error ZeroDeposit();

    event ChannelOpened(
        bytes32 indexed channelId,
        address indexed payer,
        address indexed payee,
        address operator,
        address token,
        address authorizedSigner,
        bytes32 salt,
        uint128 deposit
    );

    event Settled(
        bytes32 indexed channelId,
        address indexed payer,
        address indexed payee,
        uint128 cumulativeAmount,
        uint128 deltaPaid,
        uint128 newSettled
    );

    event SettlementOperatorAssigned(bytes32 indexed channelId, address indexed operator);

    event TopUp(
        bytes32 indexed channelId,
        address indexed payer,
        address indexed payee,
        uint128 additionalDeposit,
        uint128 newDeposit
    );

    event CloseRequested(
        bytes32 indexed channelId, address indexed payer, address indexed payee, uint256 closeGraceEnd
    );

    event CloseRequestCancelled(bytes32 indexed channelId, address indexed payer, address indexed payee);

    event ChannelClosed(
        bytes32 indexed channelId,
        address indexed payer,
        address indexed payee,
        uint128 finalSettled,
        uint128 paidToPayee,
        uint128 refundedToPayer,
        bool forced
    );

    function CONTRACT_NAME() external view returns (string memory);
    function CONTRACT_VERSION() external view returns (uint64);
    function NAME() external view returns (string memory);
    function VERSION() external view returns (string memory);
    function CLOSE_GRACE_PERIOD() external view returns (uint64);
    function MAX_BATCH_SIZE() external view returns (uint256);

    function usdc() external view returns (address);
    function eurc() external view returns (address);
    function isAcceptedToken(address token) external view returns (bool);
    function closeGracePeriod() external view returns (uint64);

    function computeChannelId(address payer, address payee, address token, bytes32 salt, address authorizedSigner)
        external
        view
        returns (bytes32);

    function computeChannelIdWithOperator(
        address payer,
        address payee,
        address operator,
        address token,
        bytes32 salt,
        address authorizedSigner
    ) external view returns (bytes32);

    function getVoucherDigest(bytes32 channelId, uint128 cumulativeAmount) external view returns (bytes32);
    function voucherDigest(bytes32 channelId, uint128 cumulativeAmount) external view returns (bytes32);
    function domainSeparator() external view returns (bytes32);
    function channels(bytes32 channelId) external view returns (Channel memory);
    function getChannel(bytes32 channelId) external view returns (Channel memory);
    function getChannelsBatch(bytes32[] calldata channelIds) external view returns (Channel[] memory);
    function getChannelState(bytes32 channelId)
        external
        view
        returns (ChannelStatus status, uint128 remaining, uint256 closeGraceEndsAt);

    /// @notice Opens a channel whose payee submits settlement transactions directly.
    function open(address payee, address token, uint128 deposit, bytes32 salt, address authorizedSigner)
        external
        returns (bytes32 channelId);

    /// @notice Opens a channel with a distinct settlement submitter.
    function openWithOperator(
        address payee,
        address operator,
        address token,
        uint128 deposit,
        bytes32 salt,
        address authorizedSigner
    ) external returns (bytes32 channelId);

    function settle(bytes32 channelId, uint128 cumulativeAmount, bytes calldata signature) external;
    function settleBatch(Settlement[] calldata settlements) external;
    function topUp(bytes32 channelId, uint128 additionalDeposit) external;
    function close(bytes32 channelId, uint128 cumulativeAmount, bytes calldata signature) external;
    function requestClose(bytes32 channelId) external;
    function withdraw(bytes32 channelId) external;

    function pauseFunding() external;
    function unpauseFunding() external;
    function paused() external view returns (bool);
}
