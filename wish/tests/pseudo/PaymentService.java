package com.example.payments;

import java.math.BigDecimal;
import java.time.Clock;
import java.time.Instant;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.stream.Collectors;

import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RestController;

@RestController
public class PaymentService {

    private static final BigDecimal LIMIT = new BigDecimal("10000");

    private final PaymentRepository payments;
    private final FraudCheck fraud;
    private final Clock clock;

    public PaymentService(PaymentRepository payments, FraudCheck fraud, Clock clock) {
        this.payments = Objects.requireNonNull(payments);
        this.fraud = Objects.requireNonNull(fraud);
        this.clock = clock;
    }

    @PostMapping("/accounts/{accountId}/payments")
    public ResponseEntity<PaymentView> pay(@PathVariable String accountId, @RequestBody PaymentRequest request) {
        if (request == null || request.amount() == null) {
            return ResponseEntity.badRequest().build();
        }
        if (request.amount().signum() <= 0) {
            return ResponseEntity.unprocessableEntity().build();
        } else if (request.amount().compareTo(LIMIT) > 0) {
            if (!request.approvedByManager()) {
                return ResponseEntity.status(403).build();
            }
        }
        Payment payment = Payment.builder()
                .accountId(accountId)
                .amount(request.amount())
                .currency(request.currency())
                .reference(request.reference())
                .createdAt(Instant.now(clock))
                .build();
        Verdict verdict = fraud.check(payment);
        PaymentView view = switch (verdict) {
            case Verdict.Clear clear -> PaymentView.of(payments.save(payment.booked()));
            case Verdict.Review review -> PaymentView.of(payments.save(payment.held(review.reason())));
            case Verdict.Block block -> throw new PaymentBlockedException(block.rule());
        };
        return ResponseEntity.ok(view);
    }

    /** Totals per currency of the booked payments of an account. */
    public Map<String, BigDecimal> totals(String accountId) {
        return payments.findByAccount(accountId).stream()
                .filter(Payment::isBooked)
                .collect(Collectors.groupingBy(Payment::currency,
                        Collectors.reducing(BigDecimal.ZERO, Payment::amount, BigDecimal::add)));
    }

    public List<Payment> settle(List<Payment> held, BigDecimal budget) {
        List<Payment> settled = new ArrayList<>();
        BigDecimal left = budget;
        for (Payment p : held) {
            if (p.amount().compareTo(left) > 0) {
                continue;
            }
            // Oldest first: the list arrives sorted by createdAt.
            payments.save(p.booked());
            settled.add(p);
            left = left.subtract(p.amount());
        }
        return settled;
    }
}
