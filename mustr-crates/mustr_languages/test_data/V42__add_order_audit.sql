-- Orders, with an audit trail and a status.
CREATE TABLE [dbo].[Orders] (
    [Id]         INT IDENTITY(1, 1) NOT NULL PRIMARY KEY,
    [CustomerId] INT NOT NULL,
    [Total]      DECIMAL(18, 2) NOT NULL,
    [Status]     NVARCHAR(20) NOT NULL CONSTRAINT [DF_Orders_Status] DEFAULT N'Pending',
    [CreatedAt]  DATETIME2(3) NOT NULL DEFAULT SYSUTCDATETIME()
);
GO

IF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE name = 'IX_Orders_CustomerId')
BEGIN
    CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] ([CustomerId]) INCLUDE ([Total]);
END
GO

MERGE INTO [dbo].[OrderTotals] AS target
USING (
    SELECT [CustomerId], SUM([Total]) AS [Total]
    FROM [dbo].[Orders]
    GROUP BY [CustomerId]
) AS source
ON target.[CustomerId] = source.[CustomerId]
WHEN MATCHED THEN
    UPDATE SET target.[Total] = source.[Total]
WHEN NOT MATCHED BY TARGET THEN
    INSERT ([CustomerId], [Total]) VALUES (source.[CustomerId], source.[Total]);
GO
