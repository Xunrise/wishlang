package com.example.staging.application.service;

import com.example.audit.application.port.inbound.AuditCorrelationPort;
import com.example.staging.application.port.inbound.HandOverPort;
import com.example.staging.application.port.inbound.StagedMessageStatusPort;
import com.example.staging.application.port.outbound.SpiHandOverLogPort;
import com.example.staging.application.port.outbound.StagedMessageHandler;
import com.example.staging.domain.ClaimedMessage;
import com.example.staging.domain.HandOverNotAllowedException;
import com.example.staging.domain.HandOverOutcome;
import com.example.staging.domain.HandOverRefusedException;
import com.example.staging.domain.StagedMessageNotFoundException;
import com.example.staging.domain.StagedMessageStatus;
import com.example.staging.domain.StagedReceipt;
import com.example.staging.domain.StagingStoreUnavailableException;

import java.util.Objects;
import java.util.Optional;
import java.util.UUID;

/**
 * The only path of a staged message into the domain, on its first run and on every replay:
 * claim it ({@code RECEIVED|FAILED -> PROCESSING}, counting the attempt), route it to the handler
 * registered for its route key, let the handler call its feature's inbound port with the staged id
 * as the audit correlation id, and record the outcome.
 *
 * <p>A handler's {@link HandOverRefusedException} is REJECTED, for good; any other exception is
 * FAILED and may be handed over again. When the outcome cannot be recorded the message stays
 * PROCESSING, for a person to resolve: its port may have committed.
 */
public class HandOverService implements HandOverPort {

    /** The reason of a FAILED hand-over whose handler threw anything but a refusal. */
    public static final String TECHNICAL_FAILURE = "TECHNICAL_FAILURE";
    /** The reason of a FAILED hand-over whose route key has no handler any more. */
    public static final String NO_HANDLER = "NO_HANDLER";

    private final StagedMessageStatusPort statusPort;
    private final StagedMessageHandlers handlers;
    private final AuditCorrelationPort auditCorrelation;
    private final SpiHandOverLogPort log;

    public HandOverService(StagedMessageStatusPort statusPort,
                           StagedMessageHandlers handlers,
                           AuditCorrelationPort auditCorrelation,
                           SpiHandOverLogPort log) {
        this.statusPort = Objects.requireNonNull(statusPort, "statusPort");
        this.handlers = Objects.requireNonNull(handlers, "handlers");
        this.auditCorrelation = Objects.requireNonNull(auditCorrelation, "auditCorrelation");
        this.log = Objects.requireNonNull(log, "log");
    }

    /**
     * @throws StagedMessageNotFoundException when no staged message has the id
     * @throws HandOverNotAllowedException when the message is not RECEIVED or FAILED, or has no route
     * @throws StagingStoreUnavailableException when the message cannot be claimed
     */
    @Override
    public StagedReceipt handOver(UUID id) {
        Objects.requireNonNull(id, "id");
        ClaimedMessage claimed = statusPort.claim(id).orElseThrow(() -> notClaimable(id));
        Ended ended = handOverClaimed(claimed);
        try {
            statusPort.record(id, ended.outcome());
        } catch (StagingStoreUnavailableException unrecorded) {
            report(claimed, StagedMessageStatus.PROCESSING, null);
            return new StagedReceipt(id, StagedMessageStatus.PROCESSING, claimed.attempts(), null, false);
        }
        StagedMessageStatus status = statusOf(ended.outcome());
        report(claimed, status, ended.cause());
        return new StagedReceipt(id, status, claimed.attempts(), ended.outcome(), false);
    }

    private Ended handOverClaimed(ClaimedMessage claimed) {
        Optional<StagedMessageHandler> handler = handlers.forRouteKey(claimed.routeKey());
        if (handler.isEmpty()) {
            return new Ended(new HandOverOutcome.Failed(NO_HANDLER), NO_HANDLER);
        }
        try {
            String objectId = auditCorrelation.correlated(claimed.id(),
                    () -> handler.get().handle(claimed.originalContent(), claimed.routeArgs()));
            return new Ended(new HandOverOutcome.HandedOver(objectId), null);
        } catch (HandOverRefusedException refused) {
            return new Ended(new HandOverOutcome.Rejected(refused.fields()), null);
        } catch (RuntimeException failure) {
            return new Ended(new HandOverOutcome.Failed(TECHNICAL_FAILURE), failure.getClass().getSimpleName());
        }
    }

    private RuntimeException notClaimable(UUID id) {
        return statusPort.receipt(id)
                .<RuntimeException>map(receipt -> new HandOverNotAllowedException(receipt.status()))
                .orElseGet(StagedMessageNotFoundException::new);
    }

    private void report(ClaimedMessage claimed, StagedMessageStatus status, String cause) {
        log.handedOver(claimed.id(), claimed.routeKey(), claimed.channel(), status, claimed.attempts(), cause);
    }

    private static StagedMessageStatus statusOf(HandOverOutcome outcome) {
        return switch (outcome) {
            case HandOverOutcome.HandedOver handedOver -> StagedMessageStatus.HANDED_OVER;
            case HandOverOutcome.Rejected rejected -> StagedMessageStatus.REJECTED;
            case HandOverOutcome.Failed failed -> StagedMessageStatus.FAILED;
        };
    }

    /** How the hand-over ended, and for a FAILED one what failed it, for the log only. */
    private record Ended(HandOverOutcome outcome, String cause) {
    }
}
