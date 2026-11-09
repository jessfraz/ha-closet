CREATE INDEX items_active_variant
ON items(json_extract(body, '$.identity.variant_id'), sort_brand, sort_name, id)
WHERE archived = 0;
