-- A quiet routine's bot may end a run with no news: the run stays in the
-- routine's history but posts nothing the owner is alerted to.
ALTER TABLE routines ADD COLUMN quiet INTEGER NOT NULL DEFAULT false CHECK (quiet IN (false, true));

ALTER TABLE routine_runs ADD COLUMN report TEXT NOT NULL DEFAULT 'news'
    CHECK (report IN ('news', 'no_news'));

-- A run that involved the owner or acted outside the bot reports as news
-- however it ended: no-news only holds for a run that completed without a
-- card, notice, steer, draft, delegation, mail or social action.
CREATE TRIGGER routine_runs_news_when_involved AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status = 'completed' BEGIN
    UPDATE routine_runs SET report = 'news'
    WHERE turn_id = NEW.id AND report = 'no_news' AND (
        EXISTS (SELECT 1 FROM owner_questions p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM computer_handoffs p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM secret_requests p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM skill_installs p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM outgoing_drafts p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM owner_notices p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM social_setups p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM social_actions p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM connector_actions p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM delegations p WHERE p.root_turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM turn_steers p WHERE p.into_turn_id = NEW.id));
END;
