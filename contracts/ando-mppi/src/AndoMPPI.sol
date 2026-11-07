// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.30;

import {AccessControlUpgradeable} from "@openzeppelin/contracts-upgradeable/access/AccessControlUpgradeable.sol";
import {EIP712Upgradeable} from "@openzeppelin/contracts-upgradeable/utils/cryptography/EIP712Upgradeable.sol";
import {PausableUpgradeable} from "@openzeppelin/contracts-upgradeable/utils/PausableUpgradeable.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Initializable} from "@openzeppelin/contracts/proxy/utils/Initializable.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {SignatureChecker} from "@openzeppelin/contracts/utils/cryptography/SignatureChecker.sol";

/// @title AndoMPPI
/// @notice Upgrade-safe Arc escrow for MPP-I cumulative payment channels.
/// @dev The contract stores only public channel and settlement state. HTTP credentials,
/// bearer secrets, application metadata, and inference content must never be submitted.
contract AndoMPPI is Initializable, AccessControlUpgradeable, EIP712Upgradeable, PausableUpgradeable, ReentrancyGuard {
    using SafeERC20 for IERC20;

    string public constant CONTRACT_NAME = "AndoMPPI";
    uint64 public constant CONTRACT_VERSION = 1;
    string public constant NAME = "AndoMPPI";
    string public constant VERSION = "1";

    uint64 public constant CLOSE_GRACE_PERIOD = 15 minutes;
    uint256 public constant MAX_BATCH_SIZE = 256;
    bytes32 public constant PAUSER_ROLE = keccak256("PAUSER_ROLE");
    bytes32 public constant VOUCHER_TYPEHASH = keccak256("Voucher(bytes32 channelId,uint128 cumulativeAmount)");

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

    /// @custom:storage-location erc7201:ando.storage.AndoMPPI
    struct AndoMppiStorage {
        mapping(bytes32 channelId => Channel channel) channels;
        address usdc;
        address eurc;
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

    // keccak256(abi.encode(uint256(keccak256("ando.storage.AndoMPPI")) - 1)) & ~bytes32(uint256(0xff))
    bytes32 private constant ANDO_MPPI_STORAGE_LOCATION =
        0x749e4ee8160bc9784963c50f14c3f30d07027b4e0c622c54072491afb31bc300;

    /// @dev Locks the standalone implementation. State is initialized only through the proxy.
    /// @custom:oz-upgrades-unsafe-allow constructor
    constructor() {
        _disableInitializers();
    }

    /// @notice Initializes one proxy with its two accepted Arc currencies and governance roles.
    /// @param usdc_ Official USDC contract for the target Arc network.
    /// @param eurc_ Official EURC contract for the target Arc network.
    /// @param governance_ Governance address allowed to manage roles and unpause funding.
    /// @param guardian_ Emergency address allowed to pause new funding.
    function initialize(address usdc_, address eurc_, address governance_, address guardian_) external initializer {
        if (governance_ == address(0) || guardian_ == address(0)) revert ZeroAddress();
        if (
            usdc_ == address(0) || eurc_ == address(0) || usdc_ == eurc_ || usdc_.code.length == 0
                || eurc_.code.length == 0
        ) revert InvalidCurrency();

        __AccessControl_init();
        __EIP712_init(NAME, VERSION);
        __Pausable_init();

        AndoMppiStorage storage $ = _getAndoMppiStorage();
        $.usdc = usdc_;
        $.eurc = eurc_;
        _grantRole(DEFAULT_ADMIN_ROLE, governance_);
        _grantRole(PAUSER_ROLE, guardian_);
    }

    /// @notice Pauses only new channel funding. Settlement and exits remain available.
    function pauseFunding() external onlyRole(PAUSER_ROLE) {
        _pause();
    }

    /// @notice Resumes channel funding after governance review.
    function unpauseFunding() external onlyRole(DEFAULT_ADMIN_ROLE) {
        _unpause();
    }

    function usdc() external view returns (address) {
        return _getAndoMppiStorage().usdc;
    }

    function eurc() external view returns (address) {
        return _getAndoMppiStorage().eurc;
    }

    function isAcceptedToken(address token) public view returns (bool) {
        AndoMppiStorage storage $ = _getAndoMppiStorage();
        return token == $.usdc || token == $.eurc;
    }

    function closeGracePeriod() external pure returns (uint64) {
        return CLOSE_GRACE_PERIOD;
    }

    /// @notice Computes the identifier for a channel settled directly by its payee.
    function computeChannelId(address payer, address payee, address token, bytes32 salt, address authorizedSigner)
        public
        view
        returns (bytes32)
    {
        bytes memory encoded = abi.encode(payer, payee, token, salt, authorizedSigner, address(this), block.chainid);
        // forge-lint: disable-next-line(asm-keccak256)
        return keccak256(encoded);
    }

    /// @notice Computes the identifier for a channel with a separate settlement operator.
    /// @dev Binding the immutable operator prevents two permission sets from sharing an identifier.
    function computeChannelIdWithOperator(
        address payer,
        address payee,
        address operator,
        address token,
        bytes32 salt,
        address authorizedSigner
    ) public view returns (bytes32) {
        bytes memory encoded = abi.encode(
            payer, payee, token, salt, authorizedSigner, operator, address(this), block.chainid
        );
        // forge-lint: disable-next-line(asm-keccak256)
        return keccak256(encoded);
    }

    function getVoucherDigest(bytes32 channelId, uint128 cumulativeAmount) public view returns (bytes32) {
        return _hashTypedDataV4(keccak256(abi.encode(VOUCHER_TYPEHASH, channelId, cumulativeAmount)));
    }

    function voucherDigest(bytes32 channelId, uint128 cumulativeAmount) external view returns (bytes32) {
        return getVoucherDigest(channelId, cumulativeAmount);
    }

    function domainSeparator() external view returns (bytes32) {
        return _domainSeparatorV4();
    }

    function channels(bytes32 channelId) external view returns (Channel memory) {
        return _getAndoMppiStorage().channels[channelId];
    }

    function getChannel(bytes32 channelId) public view returns (Channel memory channel) {
        channel = _getAndoMppiStorage().channels[channelId];
        if (channel.payer == address(0)) revert ChannelNotFound();
    }

    function getChannelsBatch(bytes32[] calldata channelIds) external view returns (Channel[] memory result) {
        uint256 length = channelIds.length;
        if (length > MAX_BATCH_SIZE) revert BatchTooLarge();
        result = new Channel[](length);
        AndoMppiStorage storage $ = _getAndoMppiStorage();
        for (uint256 index; index < length; ++index) {
            result[index] = $.channels[channelIds[index]];
        }
    }

    /// @notice Returns lifecycle status and derived balances without reverting for an unknown identifier.
    function getChannelState(bytes32 channelId)
        external
        view
        returns (ChannelStatus status, uint128 remaining, uint256 closeGraceEndsAt)
    {
        Channel storage channel = _getAndoMppiStorage().channels[channelId];
        if (channel.payer == address(0)) return (ChannelStatus.None, 0, 0);
        if (channel.finalized) return (ChannelStatus.Finalized, 0, 0);

        remaining = channel.deposit - channel.settled;
        if (channel.closeRequestedAt == 0) return (ChannelStatus.Open, remaining, 0);
        return (ChannelStatus.Closing, remaining, uint256(channel.closeRequestedAt) + CLOSE_GRACE_PERIOD);
    }

    /// @notice Opens a channel whose payee submits settlement transactions directly.
    function open(address payee, address token, uint128 deposit, bytes32 salt, address authorizedSigner)
        external
        whenNotPaused
        nonReentrant
        returns (bytes32 channelId)
    {
        channelId = _open(msg.sender, payee, address(0), token, deposit, salt, authorizedSigner);
    }

    /// @notice Opens a channel with a separate address permitted to submit settlement transactions.
    /// @dev `address(0)` disables the operator. The operator can never redirect funds:
    /// every settlement is paid to `payee`.
    function openWithOperator(
        address payee,
        address operator,
        address token,
        uint128 deposit,
        bytes32 salt,
        address authorizedSigner
    ) external whenNotPaused nonReentrant returns (bytes32 channelId) {
        if (operator == address(0)) revert ZeroAddress();
        channelId = _open(msg.sender, payee, operator, token, deposit, salt, authorizedSigner);
    }

    function _open(
        address payer,
        address payee,
        address operator,
        address token,
        uint128 deposit,
        bytes32 salt,
        address authorizedSigner
    ) private returns (bytes32 channelId) {
        if (payee == address(0)) revert ZeroAddress();
        if (!isAcceptedToken(token)) revert InvalidCurrency();
        if (deposit == 0) revert ZeroDeposit();

        channelId = operator == address(0)
            ? computeChannelId(payer, payee, token, salt, authorizedSigner)
            : computeChannelIdWithOperator(payer, payee, operator, token, salt, authorizedSigner);
        AndoMppiStorage storage $ = _getAndoMppiStorage();
        if ($.channels[channelId].payer != address(0)) revert ChannelAlreadyExists();

        $.channels[channelId] = Channel({
            payer: payer,
            payee: payee,
            operator: operator,
            token: token,
            authorizedSigner: authorizedSigner,
            deposit: deposit,
            settled: 0,
            closeRequestedAt: 0,
            finalized: false
        });
        _pullExact(token, payer, deposit);
        emit ChannelOpened(channelId, payer, payee, operator, token, authorizedSigner, salt, deposit);
        if (operator != address(0)) emit SettlementOperatorAssigned(channelId, operator);
    }

    /// @notice Pays the provider the increase authorized by a cumulative voucher.
    /// @dev The voucher signer is `authorizedSigner`, or `payer` when it is zero.
    function settle(bytes32 channelId, uint128 cumulativeAmount, bytes calldata signature) external nonReentrant {
        _settle(channelId, cumulativeAmount, signature);
    }

    /// @notice Atomically settles multiple cumulative vouchers.
    /// @dev Any invalid item reverts the entire batch; successful items emit the normal `Settled` event.
    function settleBatch(Settlement[] calldata settlements) external nonReentrant {
        uint256 length = settlements.length;
        if (length > MAX_BATCH_SIZE) revert BatchTooLarge();
        for (uint256 index; index < length; ++index) {
            Settlement calldata item = settlements[index];
            _settle(item.channelId, item.cumulativeAmount, item.signature);
        }
    }

    function _settle(bytes32 channelId, uint128 cumulativeAmount, bytes calldata signature) private {
        Channel storage channel = _activeChannel(channelId);
        _requirePayeeOrOperator(channel);
        _verifyAdvance(channelId, channel, cumulativeAmount, signature);

        uint128 delta = cumulativeAmount - channel.settled;
        channel.settled = cumulativeAmount;
        IERC20(channel.token).safeTransfer(channel.payee, delta);
        emit Settled(channelId, channel.payer, channel.payee, cumulativeAmount, delta, cumulativeAmount);
    }

    /// @notice Adds payer funds and cancels a pending close request.
    function topUp(bytes32 channelId, uint128 additionalDeposit) external whenNotPaused nonReentrant {
        Channel storage channel = _activeChannel(channelId);
        if (msg.sender != channel.payer) revert NotPayer();
        if (additionalDeposit == 0) revert ZeroDeposit();

        bool cancelledClose = channel.closeRequestedAt != 0;
        if (uint256(channel.deposit) + additionalDeposit > type(uint128).max) revert DepositOverflow();

        _pullExact(channel.token, msg.sender, additionalDeposit);
        channel.deposit += additionalDeposit;
        if (cancelledClose) {
            channel.closeRequestedAt = 0;
            emit CloseRequestCancelled(channelId, channel.payer, channel.payee);
        }
        emit TopUp(channelId, channel.payer, channel.payee, additionalDeposit, channel.deposit);
    }

    /// @notice Finalizes a channel, pays any final voucher delta, and refunds the payer.
    /// @dev A cumulative amount at or below `settled` is a provider forfeit path
    /// and requires no voucher because it cannot increase provider payment.
    function close(bytes32 channelId, uint128 cumulativeAmount, bytes calldata signature) external nonReentrant {
        Channel storage channel = _activeChannel(channelId);
        _requirePayeeOrOperator(channel);
        if (cumulativeAmount > channel.deposit) revert AmountExceedsDeposit();

        uint128 finalAmount = channel.settled;
        if (cumulativeAmount > channel.settled) {
            _verifyVoucher(channelId, channel, cumulativeAmount, signature);
            finalAmount = cumulativeAmount;
        }
        uint128 payeeDelta = finalAmount - channel.settled;
        uint128 refund = channel.deposit - finalAmount;
        channel.settled = finalAmount;
        channel.finalized = true;
        channel.closeRequestedAt = 0;

        if (payeeDelta != 0) IERC20(channel.token).safeTransfer(channel.payee, payeeDelta);
        if (refund != 0) IERC20(channel.token).safeTransfer(channel.payer, refund);
        emit ChannelClosed(channelId, channel.payer, channel.payee, finalAmount, payeeDelta, refund, false);
    }

    /// @notice Starts the payer's forced-close grace period.
    /// @dev Repeated calls are idempotent and cannot restart the timer.
    function requestClose(bytes32 channelId) external {
        Channel storage channel = _activeChannel(channelId);
        if (msg.sender != channel.payer) revert NotPayer();
        if (channel.closeRequestedAt != 0) return;

        uint64 requestedAt = uint64(block.timestamp);
        if (requestedAt == 0) requestedAt = 1;
        channel.closeRequestedAt = requestedAt;
        emit CloseRequested(channelId, channel.payer, channel.payee, uint256(requestedAt) + CLOSE_GRACE_PERIOD);
    }

    /// @notice Finalizes after the grace period and refunds all unsettled funds.
    /// @dev Settlement remains available during the grace period.
    function withdraw(bytes32 channelId) external nonReentrant {
        Channel storage channel = _activeChannel(channelId);
        if (msg.sender != channel.payer) revert NotPayer();
        uint64 requestedAt = channel.closeRequestedAt;
        if (requestedAt == 0 || block.timestamp < uint256(requestedAt) + CLOSE_GRACE_PERIOD) {
            revert CloseNotReady();
        }

        uint128 refund = channel.deposit - channel.settled;
        channel.finalized = true;
        channel.closeRequestedAt = 0;
        if (refund != 0) IERC20(channel.token).safeTransfer(channel.payer, refund);
        emit ChannelClosed(channelId, channel.payer, channel.payee, channel.settled, 0, refund, true);
    }

    function _activeChannel(bytes32 channelId) private view returns (Channel storage channel) {
        channel = _getAndoMppiStorage().channels[channelId];
        if (channel.payer == address(0)) revert ChannelNotFound();
        if (channel.finalized) revert ChannelFinalized();
    }

    function _verifyAdvance(
        bytes32 channelId,
        Channel storage channel,
        uint128 cumulativeAmount,
        bytes calldata signature
    ) private view {
        if (cumulativeAmount <= channel.settled) revert AmountNotIncreasing();
        if (cumulativeAmount > channel.deposit) revert AmountExceedsDeposit();
        _verifyVoucher(channelId, channel, cumulativeAmount, signature);
    }

    function _requirePayeeOrOperator(Channel storage channel) private view {
        if (msg.sender != channel.payee && (channel.operator == address(0) || msg.sender != channel.operator)) {
            revert NotPayeeOrOperator();
        }
    }

    function _verifyVoucher(
        bytes32 channelId,
        Channel storage channel,
        uint128 cumulativeAmount,
        bytes calldata signature
    ) private view {
        address expectedSigner = channel.authorizedSigner == address(0) ? channel.payer : channel.authorizedSigner;
        if (!SignatureChecker.isValidSignatureNow(
                expectedSigner, getVoucherDigest(channelId, cumulativeAmount), signature
            )) {
            revert InvalidSignature();
        }
    }

    function _pullExact(address token, address payer, uint128 amount) private {
        IERC20 currency = IERC20(token);
        uint256 balanceBefore = currency.balanceOf(address(this));
        currency.safeTransferFrom(payer, address(this), amount);
        uint256 balanceAfter = currency.balanceOf(address(this));
        if (balanceAfter < balanceBefore || balanceAfter - balanceBefore != amount) {
            revert TokenTransferMismatch();
        }
    }

    function _getAndoMppiStorage() private pure returns (AndoMppiStorage storage $) {
        assembly ("memory-safe") {
            $.slot := ANDO_MPPI_STORAGE_LOCATION
        }
    }
}
