// Retries outbound webhook deliveries with exponential backoff.
using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.Extensions.Logging;

namespace Acme.Webhooks;

/// <summary>A delivery that failed, and when it may be tried again.</summary>
public sealed record RetryPlan(Guid DeliveryId, int Attempt, DateTimeOffset NotBefore);

[Obsolete("Use RetryScheduler instead.")]
public sealed class RetryService<TStore>(TStore store, ILogger<RetryService<TStore>> logger)
    where TStore : class, IDeliveryStore
{
    private const int MaxAttempts = 5;
    private static readonly TimeSpan BaseDelay = TimeSpan.FromSeconds(2);

    public async Task<IReadOnlyList<RetryPlan>> PlanRetriesAsync(
        DateTimeOffset now,
        CancellationToken cancellationToken = default)
    {
        var failed = await store.GetFailedAsync(cancellationToken).ConfigureAwait(false);
        var plans = failed
            .Where(d => d.Attempts < MaxAttempts && d.LastError is not null)
            .OrderBy(d => d.FailedAt)
            .Select(d => new RetryPlan(d.Id, d.Attempts + 1, now + Backoff(d.Attempts)))
            .ToList();

        logger.LogInformation($"Planned {plans.Count} retries of {failed.Count} failures");
        return plans;
    }

    public static TimeSpan Backoff(int attempt) => attempt switch
    {
        <= 0 => TimeSpan.Zero,
        >= MaxAttempts => TimeSpan.FromMinutes(10),
        _ => BaseDelay * Math.Pow(2, attempt),
    };

    private string? Describe(Delivery? delivery) =>
        delivery?.Endpoint is { } endpoint ? $"{endpoint.Host}:{endpoint.Port}" : null;
}
