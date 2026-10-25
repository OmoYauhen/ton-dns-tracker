// @generated automatically by Diesel CLI.

diesel::table! {
    discovery_progress (id) {
        id -> Bool,
        next_lt -> Int8,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    domain_events (id) {
        id -> Int8,
        domain_address -> Bytea,
        lt -> Int8,
        event_time -> Timestamptz,
        kind -> Text,
        actor_address -> Nullable<Bytea>,
        amount -> Nullable<Int8>,
        details -> Jsonb,
    }
}

diesel::table! {
    domains (address) {
        address -> Bytea,
        item_index -> Numeric,
        name -> Text,
        namespace -> Text,
        owner_address -> Nullable<Bytea>,
        last_fill_up_time -> Timestamptz,
        auction_max_bid_address -> Nullable<Bytea>,
        auction_max_bid_amount -> Nullable<Int8>,
        auction_end_time -> Nullable<Timestamptz>,
        wallet_address -> Nullable<Bytea>,
        site_adnl -> Nullable<Bytea>,
        storage_bag_id -> Nullable<Bytea>,
        next_resolver -> Nullable<Bytea>,
        extra_records -> Nullable<Jsonb>,
        code_hash -> Bytea,
        last_transaction_lt -> Int8,
        updated_at -> Timestamptz,
        events_synced_lt -> Int8,
    }
}

diesel::joinable!(domain_events -> domains (domain_address));

diesel::allow_tables_to_appear_in_same_query!(discovery_progress, domain_events, domains,);
