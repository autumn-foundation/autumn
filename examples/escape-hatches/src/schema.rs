// Diesel tables. They match `migrations/20261005163412_create_stockroom`.
diesel::table! {
    products (id) {
        id -> Int8,
        sku -> Text,
        name -> Text,
        category -> Text,
        stock -> Int4,
        price_cents -> Int8,
    }
}

diesel::table! {
    orders (id) {
        id -> Int8,
        order_ref -> Text,
    }
}

diesel::table! {
    order_lines (id) {
        id -> Int8,
        order_id -> Int8,
        product_id -> Int8,
        sku -> Text,
        quantity -> Int4,
    }
}

diesel::joinable!(order_lines -> orders (order_id));
diesel::joinable!(order_lines -> products (product_id));
diesel::allow_tables_to_appear_in_same_query!(products, orders, order_lines);
