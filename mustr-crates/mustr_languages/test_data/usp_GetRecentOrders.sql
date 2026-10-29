-- Recent orders for a customer, newest first, with each order's rank.
CREATE OR ALTER PROCEDURE [dbo].[usp_GetRecentOrders]
    @CustomerId INT,
    @Since DATETIME2(3) = NULL,
    @Top INT = 50
AS
BEGIN
    SET NOCOUNT ON;
    DECLARE @Cutoff DATETIME2(3) = COALESCE(@Since, DATEADD(DAY, -30, SYSUTCDATETIME()));

    WITH [Recent] AS (
        SELECT o.[Id], o.[Total], o.[Status], o.[CreatedAt],
               ROW_NUMBER() OVER (PARTITION BY o.[Status] ORDER BY o.[CreatedAt] DESC) AS [Rank],
               SUM(o.[Total]) OVER (ORDER BY o.[CreatedAt] ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS [RunningTotal]
        FROM [dbo].[Orders] AS o
        WHERE o.[CustomerId] = @CustomerId AND o.[CreatedAt] >= @Cutoff
    )
    SELECT TOP (@Top) [Id], [Total], [Status], [Rank], [RunningTotal]
    FROM [Recent]
    ORDER BY [CreatedAt] DESC;

    BEGIN TRY
        BEGIN TRANSACTION;
        UPDATE [dbo].[Customers] SET [LastViewedAt] = SYSUTCDATETIME() WHERE [Id] = @CustomerId;
        COMMIT TRANSACTION;
    END TRY
    BEGIN CATCH
        IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;
        THROW;
    END CATCH
END
GO
