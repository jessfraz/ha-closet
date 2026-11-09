-- Area tracking is derived from configured areas, not an independent switch.
ALTER TABLE tracking_state DROP COLUMN enabled;
