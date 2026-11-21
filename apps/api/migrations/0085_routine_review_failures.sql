-- How many times reviewing an ended run has failed. After a bounded number
-- the run is released: marked reviewed without its outcome counted, with a
-- notice to the owner, so it stops holding its conversation's next turn.
ALTER TABLE routine_runs ADD COLUMN review_failures INTEGER NOT NULL DEFAULT 0
    CHECK (review_failures >= 0);
