"""Create the fictional `ferox_demo` database used for the README screenshots.

    pip install psycopg2-binary
    FEROX_DEMO_DSN="host=localhost user=postgres password=..." python scripts/demo_db.py

Drops and recreates `ferox_demo` — an e-commerce schema (`shop`) plus an
`analytics` schema with enums, identity columns, JSONB, FKs, a view, a
materialized view, functions, a trigger, and a few thousand rows of made-up data.
"""
import os

import psycopg2

DSN = os.environ.get("FEROX_DEMO_DSN", "host=localhost user=postgres")

admin = psycopg2.connect(DSN + " dbname=postgres")
admin.autocommit = True
with admin.cursor() as c:
    c.execute("DROP DATABASE IF EXISTS ferox_demo WITH (FORCE)")
    c.execute("CREATE DATABASE ferox_demo")
admin.close()

SQL = r"""
SELECT setseed(0.42);

CREATE SCHEMA shop;
CREATE SCHEMA analytics;

CREATE TYPE shop.customer_tier AS ENUM ('bronze', 'silver', 'gold', 'platinum');
CREATE TYPE shop.order_status AS ENUM ('pending', 'paid', 'shipped', 'delivered', 'cancelled', 'refunded');

CREATE TABLE shop.customers (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    full_name   text NOT NULL,
    email       text NOT NULL UNIQUE,
    country     char(2) NOT NULL,
    city        text,
    tier        shop.customer_tier NOT NULL DEFAULT 'bronze',
    is_active   boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE shop.categories (
    id        int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name      text NOT NULL UNIQUE,
    parent_id int REFERENCES shop.categories(id)
);

CREATE TABLE shop.products (
    id          int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    sku         text NOT NULL UNIQUE,
    name        text NOT NULL,
    category_id int NOT NULL REFERENCES shop.categories(id),
    price       numeric(10,2) NOT NULL CHECK (price > 0),
    stock       int NOT NULL DEFAULT 0,
    attributes  jsonb NOT NULL DEFAULT '{}',
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE shop.orders (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    customer_id  bigint NOT NULL REFERENCES shop.customers(id),
    status       shop.order_status NOT NULL DEFAULT 'pending',
    ordered_at   timestamptz NOT NULL,
    shipped_at   timestamptz,
    total        numeric(12,2) NOT NULL DEFAULT 0,
    coupon_code  text
);

CREATE TABLE shop.order_items (
    order_id    bigint NOT NULL REFERENCES shop.orders(id) ON DELETE CASCADE,
    product_id  int NOT NULL REFERENCES shop.products(id),
    quantity    int NOT NULL CHECK (quantity > 0),
    unit_price  numeric(10,2) NOT NULL,
    PRIMARY KEY (order_id, product_id)
);

CREATE TABLE shop.reviews (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    product_id  int NOT NULL REFERENCES shop.products(id),
    customer_id bigint NOT NULL REFERENCES shop.customers(id),
    rating      smallint NOT NULL CHECK (rating BETWEEN 1 AND 5),
    body        text,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE analytics.page_views (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    customer_id bigint REFERENCES shop.customers(id),
    path        text NOT NULL,
    referrer    text,
    viewed_at   timestamptz NOT NULL
);

-- ── Data ────────────────────────────────────────────────────────────────────
INSERT INTO shop.categories (name) VALUES
  ('Electronics'), ('Home & Kitchen'), ('Books'), ('Outdoor'), ('Toys'), ('Fashion');
INSERT INTO shop.categories (name, parent_id) VALUES
  ('Headphones', 1), ('Laptops', 1), ('Cookware', 2), ('Coffee', 2),
  ('Sci-Fi', 3), ('Camping', 4), ('Board Games', 5), ('Sneakers', 6);

WITH first(n) AS (SELECT unnest(ARRAY['Ada','Linus','Grace','Alan','Margaret','Dennis','Barbara','Ken',
        'Radia','Edsger','Frances','Niklaus','Hedy','Tim','Katherine','Guido','Sophie','Yukihiro',
        'Elif','Deniz','Mert','Zeynep','Lena','Mateo','Aiko','Noah','Priya','Omar','Ines','Kai'])),
     last(n) AS (SELECT unnest(ARRAY['Lovelace','Torvalds','Hopper','Turing','Hamilton','Ritchie',
        'Liskov','Thompson','Perlman','Dijkstra','Allen','Wirth','Lamarr','Berners','Johnson','Rossum',
        'Wilson','Matsumoto','Yilmaz','Kaya','Demir','Sahin','Fischer','Garcia','Tanaka','Smith',
        'Patel','Haddad','Moreau','Larsen'])),
     names AS (
        SELECT f.n || ' ' || l.n AS full_name, row_number() OVER () AS rn
        FROM first f CROSS JOIN last l
     ),
     geo(country, city) AS (VALUES ('TR','Istanbul'),('TR','Izmir'),('DE','Berlin'),('DE','Munich'),
        ('US','Seattle'),('US','Austin'),('GB','London'),('NL','Amsterdam'),('JP','Tokyo'),
        ('FR','Lyon'),('ES','Valencia'),('SE','Malmo'))
INSERT INTO shop.customers (full_name, email, country, city, tier, is_active, created_at)
SELECT full_name,
       lower(replace(full_name, ' ', '.')) || rn || '@example.com',
       g.country, g.city,
       (ARRAY['bronze','bronze','bronze','silver','silver','gold','platinum'])[1 + floor(random()*7)::int]::shop.customer_tier,
       random() > 0.07,
       now() - (random() * interval '900 days')
FROM names
CROSS JOIN LATERAL (SELECT * FROM geo ORDER BY random() + rn * 0 LIMIT 1) g
ORDER BY rn;

INSERT INTO shop.products (sku, name, category_id, price, stock, attributes, created_at)
SELECT 'SKU-' || lpad(i::text, 5, '0'),
       p.name || CASE WHEN i > 24 THEN ' ' || (ARRAY['Pro','Mini','Max','Lite','Plus'])[1 + i % 5] ELSE '' END,
       p.cat,
       round((p.base * (0.8 + random() * 0.6))::numeric, 2),
       floor(random() * 400)::int,
       jsonb_build_object('color', (ARRAY['black','white','silver','blue','red'])[1 + i % 5],
                          'weight_g', 100 + floor(random()*2000)::int,
                          'warranty_months', (ARRAY[6,12,24])[1 + i % 3]),
       now() - (random() * interval '700 days')
FROM generate_series(1, 120) i
CROSS JOIN LATERAL (
    SELECT * FROM (VALUES
      ('Noise-Cancelling Headphones', 7, 249.0), ('Wireless Earbuds', 7, 129.0),
      ('Ultrabook 14"', 8, 1399.0), ('Gaming Laptop 16"', 8, 1899.0),
      ('Cast Iron Skillet', 9, 49.0), ('Chef''s Knife', 9, 89.0),
      ('Espresso Beans 1kg', 10, 24.0), ('Pour-Over Kettle', 10, 59.0),
      ('Dune (Paperback)', 11, 14.0), ('Foundation Trilogy', 11, 32.0),
      ('2-Person Tent', 12, 179.0), ('Sleeping Bag -5°C', 12, 119.0),
      ('Catan', 13, 45.0), ('Ticket to Ride', 13, 49.0),
      ('Trail Runner Sneakers', 14, 139.0), ('Canvas High-Tops', 14, 69.0),
      ('Smart Speaker', 1, 99.0), ('Stand Mixer', 2, 399.0),
      ('Hiking Backpack 40L', 4, 149.0), ('LEGO Space Shuttle', 5, 199.0),
      ('Mechanical Keyboard', 1, 159.0), ('Air Fryer', 2, 129.0),
      ('Headlamp', 12, 29.0), ('Denim Jacket', 6, 89.0)
    ) v(name, cat, base)
    OFFSET (i - 1) % 24 LIMIT 1
) p;

INSERT INTO shop.orders (customer_id, status, ordered_at, coupon_code)
SELECT 1 + floor(random() * 900)::bigint,
       (ARRAY['delivered','delivered','delivered','delivered','shipped','paid','pending','cancelled','refunded'])
           [1 + floor(random()*9)::int]::shop.order_status,
       now() - (random()^1.6 * interval '540 days'),
       CASE WHEN random() < 0.15 THEN (ARRAY['WELCOME10','SPRING24','FREESHIP','VIP15'])[1 + floor(random()*4)::int] END
FROM generate_series(1, 12000);

UPDATE shop.orders SET shipped_at = ordered_at + (1 + random()*4) * interval '1 day'
WHERE status IN ('shipped', 'delivered');

INSERT INTO shop.order_items (order_id, product_id, quantity, unit_price)
SELECT o.id, p.id, 1 + floor(random() * 3)::int, p.price
FROM shop.orders o
CROSS JOIN LATERAL (
    SELECT id, price FROM shop.products
    WHERE random() < 0.03 + (o.id % 7) * 0.001
    LIMIT 1 + (o.id % 4)
) p
ON CONFLICT DO NOTHING;

-- Orders the random pass gave no items get one, so no order totals 0.
INSERT INTO shop.order_items (order_id, product_id, quantity, unit_price)
SELECT o.id, p.id, 1 + floor(random()*2)::int, p.price
FROM shop.orders o
CROSS JOIN LATERAL (SELECT id, price FROM shop.products ORDER BY random() + o.id*0 LIMIT 1) p
WHERE NOT EXISTS (SELECT 1 FROM shop.order_items i WHERE i.order_id = o.id);

UPDATE shop.orders o SET total = s.sum
FROM (SELECT order_id, sum(quantity * unit_price) AS sum FROM shop.order_items GROUP BY order_id) s
WHERE s.order_id = o.id;

INSERT INTO shop.reviews (product_id, customer_id, rating, body, created_at)
SELECT 1 + floor(random()*120)::int,
       1 + floor(random()*900)::bigint,
       (ARRAY[5,5,5,4,4,4,3,2,1])[1 + floor(random()*9)::int],
       (ARRAY['Exactly as described.', 'Great value for the price.', 'Arrived quickly, works perfectly.',
              'Decent, but the packaging was damaged.', 'Would buy again!', 'Not what I expected.',
              NULL])[1 + floor(random()*7)::int],
       now() - (random() * interval '400 days')
FROM generate_series(1, 3500);

INSERT INTO analytics.page_views (customer_id, path, referrer, viewed_at)
SELECT CASE WHEN random() < 0.6 THEN 1 + floor(random()*900)::bigint END,
       (ARRAY['/', '/products', '/products/' || (1 + floor(random()*120)::int), '/cart', '/checkout', '/account'])[1 + floor(random()*6)::int],
       (ARRAY['google', 'newsletter', 'instagram', NULL, NULL])[1 + floor(random()*5)::int],
       now() - (random() * interval '90 days')
FROM generate_series(1, 60000);

-- ── Indexes, views, functions, triggers ─────────────────────────────────────
CREATE INDEX orders_customer_id_idx ON shop.orders (customer_id);
CREATE INDEX orders_ordered_at_idx  ON shop.orders (ordered_at DESC);
CREATE INDEX order_items_product_idx ON shop.order_items (product_id);
CREATE INDEX products_attributes_gin ON shop.products USING gin (attributes);
CREATE INDEX reviews_product_idx ON shop.reviews (product_id);
CREATE INDEX page_views_viewed_at_brin ON analytics.page_views USING brin (viewed_at);

CREATE VIEW shop.v_customer_revenue AS
SELECT c.id, c.full_name, c.country, c.tier,
       count(o.id)              AS orders,
       coalesce(sum(o.total),0) AS revenue,
       max(o.ordered_at)        AS last_order_at
FROM shop.customers c
LEFT JOIN shop.orders o ON o.customer_id = c.id AND o.status NOT IN ('cancelled', 'refunded')
GROUP BY c.id;

CREATE MATERIALIZED VIEW analytics.mv_daily_sales AS
SELECT date_trunc('day', ordered_at)::date AS day,
       count(*)   AS orders,
       sum(total) AS revenue
FROM shop.orders
WHERE status NOT IN ('cancelled', 'refunded')
GROUP BY 1;

CREATE FUNCTION shop.customer_lifetime_value(p_customer bigint) RETURNS numeric
LANGUAGE sql STABLE AS $$
    SELECT coalesce(sum(total), 0) FROM shop.orders
    WHERE customer_id = p_customer AND status NOT IN ('cancelled', 'refunded')
$$;

CREATE FUNCTION shop.touch_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END $$;

CREATE TRIGGER customers_touch BEFORE UPDATE ON shop.customers
FOR EACH ROW EXECUTE FUNCTION shop.touch_updated_at();

-- Physical order = id order, so browsing a table without ORDER BY looks tidy.
CLUSTER shop.orders USING orders_pkey;
CLUSTER shop.customers USING customers_pkey;
CLUSTER shop.products USING products_pkey;
CLUSTER shop.order_items USING order_items_pkey;
REFRESH MATERIALIZED VIEW analytics.mv_daily_sales;
ANALYZE;
"""

conn = psycopg2.connect(DSN + " dbname=ferox_demo")
conn.autocommit = True
with conn.cursor() as c:
    c.execute(SQL)
    for t in ["shop.customers", "shop.products", "shop.orders", "shop.order_items", "shop.reviews", "analytics.page_views"]:
        c.execute(f"SELECT count(*) FROM {t}")
        print(t, c.fetchone()[0])
conn.close()
