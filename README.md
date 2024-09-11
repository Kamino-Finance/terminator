# Kamino lending liquidations bot

## How to run

- `.env` file:

```
CLUSTER=<rpc>
KEYPAIR=./liquidator.json
RUST_BACKTRACE=1
PROGRAM_ID=SLendK7ySfcEzyaFqy93gDnD3RtrpXJcnRwb6zFHJSh
RUST_LOG=info
LIQUIDATOR_LOOKUP_TABLE_FILE=.lookuptable.rust.mainnet-beta
```

- `ENV=.env.rust.mainnet-beta cargo run -- crank`
