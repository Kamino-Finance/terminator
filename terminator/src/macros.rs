#[macro_export]
macro_rules! readable {
    ($res:expr) => {
        ::anchor_lang::prelude::AccountMeta::new_readonly($res, false)
    };
}

#[macro_export]
macro_rules! signer {
    ($res:expr) => {
        ::anchor_lang::prelude::AccountMeta::new_readonly($res, true)
    };
}

#[macro_export]
macro_rules! writable {
    ($res:expr) => {
        ::anchor_lang::prelude::AccountMeta::new($res, false)
    };
}

#[macro_export]
macro_rules! writable_signer {
    ($res:expr) => {
        AccountMeta::new($res, true)
    };
}

#[macro_export]
macro_rules! it_event {
    ($event:expr) => {
        if let Ok(val) = std::env::var("INTEGRATION_TEST") {
            if val == "true" {
                let msg = serde_json::json!({
                    "scheme": "integration-test",
                    "type": $event
                });
                println!("{}", msg.to_string());
            }
        };
    }
}
