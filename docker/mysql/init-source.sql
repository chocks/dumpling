-- Synthetic fixture data only. Never use production data in tests.

SET NAMES utf8mb4;
SET FOREIGN_KEY_CHECKS = 0;

CREATE TABLE users (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  email VARCHAR(255) NOT NULL,
  first_name VARCHAR(100) NOT NULL,
  last_name VARCHAR(100) NOT NULL,
  phone VARCHAR(32) NULL,
  password_hash VARCHAR(255) NULL,
  auth_token VARCHAR(255) NULL,
  status VARCHAR(32) NOT NULL DEFAULT 'active',
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_users_email (email)
) ENGINE=InnoDB;

CREATE TABLE products (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  sku VARCHAR(64) NOT NULL,
  name VARCHAR(255) NOT NULL,
  price DECIMAL(10,2) NOT NULL,
  meta JSON NULL,
  PRIMARY KEY (id),
  UNIQUE KEY uq_products_sku (sku)
) ENGINE=InnoDB;

CREATE TABLE orders (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  user_id BIGINT UNSIGNED NOT NULL,
  total DECIMAL(12,2) NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  KEY idx_orders_user (user_id),
  CONSTRAINT fk_orders_user FOREIGN KEY (user_id) REFERENCES users (id)
) ENGINE=InnoDB;

CREATE TABLE order_items (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id BIGINT UNSIGNED NOT NULL,
  product_id BIGINT UNSIGNED NOT NULL,
  qty INT NOT NULL,
  PRIMARY KEY (id),
  KEY idx_items_order (order_id),
  KEY idx_items_product (product_id),
  CONSTRAINT fk_items_order FOREIGN KEY (order_id) REFERENCES orders (id),
  CONSTRAINT fk_items_product FOREIGN KEY (product_id) REFERENCES products (id)
) ENGINE=InnoDB;

CREATE TABLE payments (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  order_id BIGINT UNSIGNED NOT NULL,
  card_number VARCHAR(32) NULL,
  billing_address VARCHAR(512) NULL,
  amount DECIMAL(12,2) NOT NULL,
  PRIMARY KEY (id),
  CONSTRAINT fk_payments_order FOREIGN KEY (order_id) REFERENCES orders (id)
) ENGINE=InnoDB;

CREATE TABLE audit_logs (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  message TEXT NOT NULL,
  PRIMARY KEY (id)
) ENGINE=InnoDB;

CREATE TABLE sessions (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
  token VARCHAR(255) NOT NULL,
  PRIMARY KEY (id)
) ENGINE=InnoDB;

SET FOREIGN_KEY_CHECKS = 1;

INSERT INTO users (id, email, first_name, last_name, phone, password_hash, auth_token, status) VALUES
  (1, 'alice@example.com', 'Alice', 'Anderson', '+1-202-555-0101', 'hash-alice', 'token-alice', 'active'),
  (2, 'bob@example.com', 'Bob', 'Baker', '+1-202-555-0102', 'hash-bob', 'token-bob', 'active'),
  (3, 'carol@example.com', 'Carol', 'Clark', '+1-202-555-0103', 'hash-carol', 'token-carol', 'deleted');

INSERT INTO products (id, sku, name, price, meta) VALUES
  (1, 'SKU-1', 'Widget', 9.99, JSON_OBJECT('color', 'red')),
  (2, 'SKU-2', 'Gadget', 19.50, JSON_OBJECT('color', 'blue'));

INSERT INTO orders (id, user_id, total, created_at) VALUES
  (1, 1, 29.49, '2025-02-01 10:00:00'),
  (2, 2, 9.99, '2025-03-15 12:00:00'),
  (3, 1, 19.50, '2024-06-01 08:00:00');

INSERT INTO order_items (id, order_id, product_id, qty) VALUES
  (1, 1, 1, 1),
  (2, 1, 2, 1),
  (3, 2, 1, 1),
  (4, 3, 2, 1);

INSERT INTO payments (id, order_id, card_number, billing_address, amount) VALUES
  (1, 1, '4111111111111111', '1 Main St', 29.49),
  (2, 2, '4000000000000002', '2 Oak Ave', 9.99);

INSERT INTO audit_logs (message) VALUES ('should be excluded');
INSERT INTO sessions (token) VALUES ('session-secret');
