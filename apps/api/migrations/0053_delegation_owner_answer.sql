ALTER TABLE delegations ADD COLUMN owner_question_id BLOB REFERENCES owner_questions (id);
