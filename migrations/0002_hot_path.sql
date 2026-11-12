-- Hot-path helper: CREATED -> QUOTED (store quote) -> p_then, with both
-- journal rows, in ONE round trip and one atomic statement. The
-- orders_transition trigger still validates every state change.
-- Returns 'OK', or the current state (or 'NOT_FOUND') when the order is no
-- longer CREATED (compare-and-set lost).
CREATE FUNCTION order_quote_and_advance(p_id UUID, p_quote JSONB, p_detail JSONB, p_then TEXT)
RETURNS TEXT LANGUAGE plpgsql AS $$
DECLARE cur TEXT;
BEGIN
    UPDATE orders SET state = 'QUOTED', quote = p_quote, updated_at = now()
     WHERE id = p_id AND state = 'CREATED';
    IF NOT FOUND THEN
        SELECT state INTO cur FROM orders WHERE id = p_id;
        RETURN COALESCE(cur, 'NOT_FOUND');
    END IF;
    INSERT INTO order_events (order_id, from_state, to_state, detail)
         VALUES (p_id, 'CREATED', 'QUOTED', p_detail);
    UPDATE orders SET state = p_then, updated_at = now() WHERE id = p_id;
    INSERT INTO order_events (order_id, from_state, to_state, detail)
         VALUES (p_id, 'QUOTED', p_then, '{}'::jsonb);
    RETURN 'OK';
END $$;
