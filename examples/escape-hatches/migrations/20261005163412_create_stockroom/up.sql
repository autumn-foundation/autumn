-- Products. The CHECKs stop each write that puts stock or price out of range.
-- The limits keep `stock * price_cents` inside BIGINT.
CREATE TABLE products (
    id          BIGSERIAL PRIMARY KEY,
    sku         TEXT   NOT NULL UNIQUE,
    name        TEXT   NOT NULL,
    category    TEXT   NOT NULL,
    stock       INT    NOT NULL DEFAULT 0 CHECK (stock BETWEEN 0 AND 1000000),
    price_cents BIGINT NOT NULL CHECK (price_cents BETWEEN 0 AND 100000000)
);

CREATE INDEX idx_products_category ON products (category);

-- One row per checkout. A unique `order_ref` makes a retried checkout safe.
CREATE TABLE orders (
    id         BIGSERIAL PRIMARY KEY,
    order_ref  TEXT   NOT NULL UNIQUE
);

-- A line keeps the SKU it sold. The order reads back with no join.
CREATE TABLE order_lines (
    id         BIGSERIAL PRIMARY KEY,
    order_id   BIGINT NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    product_id BIGINT NOT NULL REFERENCES products (id),
    sku        TEXT   NOT NULL,
    quantity   INT    NOT NULL CHECK (quantity > 0)
);

CREATE INDEX idx_order_lines_order_id ON order_lines (order_id);
