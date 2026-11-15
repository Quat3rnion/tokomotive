-- sqlx applies this migration in one transaction.
ALTER TABLE users RENAME COLUMN budget_period TO old_budget_period;
ALTER TABLE users ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'none' CHECK(budget_period IN ('none','daily','weekly','monthly','quarterly','yearly'));
UPDATE users SET budget_period=old_budget_period;
ALTER TABLE users DROP COLUMN old_budget_period;
ALTER TABLE users ADD COLUMN budget_window TEXT NOT NULL DEFAULT 'calendar' CHECK(budget_window IN ('calendar','rolling'));
ALTER TABLE users ADD COLUMN budget_days INTEGER NOT NULL DEFAULT 30 CHECK(budget_days BETWEEN 1 AND 366);

ALTER TABLE teams RENAME COLUMN budget_period TO old_budget_period;
ALTER TABLE teams ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'none' CHECK(budget_period IN ('none','daily','weekly','monthly','quarterly','yearly'));
UPDATE teams SET budget_period=old_budget_period;
ALTER TABLE teams DROP COLUMN old_budget_period;
ALTER TABLE teams ADD COLUMN budget_window TEXT NOT NULL DEFAULT 'calendar' CHECK(budget_window IN ('calendar','rolling'));
ALTER TABLE teams ADD COLUMN budget_days INTEGER NOT NULL DEFAULT 30 CHECK(budget_days BETWEEN 1 AND 366);

ALTER TABLE virtual_keys RENAME COLUMN budget_period TO old_budget_period;
ALTER TABLE virtual_keys ADD COLUMN budget_period TEXT NOT NULL DEFAULT 'none' CHECK(budget_period IN ('none','daily','weekly','monthly','quarterly','yearly'));
UPDATE virtual_keys SET budget_period=old_budget_period;
ALTER TABLE virtual_keys DROP COLUMN old_budget_period;
ALTER TABLE virtual_keys ADD COLUMN budget_window TEXT NOT NULL DEFAULT 'calendar' CHECK(budget_window IN ('calendar','rolling'));
ALTER TABLE virtual_keys ADD COLUMN budget_days INTEGER NOT NULL DEFAULT 30 CHECK(budget_days BETWEEN 1 AND 366);
