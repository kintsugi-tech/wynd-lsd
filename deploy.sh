#/bin/sh
junod tx wasm instantiate 3 '{"treasury": "juno1dcugtegqgswzjvdrl4cr88vv9v75wjrssucaf2", "commission": "0.09", "owner": "juno1dcugtegqgswzjvdrl4cr88vv9v75wjrssucaf2", "validators": [["junovaloper1q5860u6ducms20qu5emqwvju63ztrl5d8t055g", "0.3"],["junovaloper12szygttgeu2u0ffvfuz0ejmnx4lqwydnztwwvy", "0.3"], ["junovaloper1h3zfnp2lcdd6ddmpyft40zegsddqtvsqyz3249", "0.4"]], "cw20_init": {"cw20_code_id": 1, "decimals": 6, "label": "dimi lsd", "name": "wyJUNO", "symbol": "wyJUNO", "initial_balances": [] }, "epoch_period": 3600, "unbond_period": 3600, "max_concurrent_unbondings": 7, "liquidity_discount": "0.03"}' --from drip --label "culo" --admin juno1dcugtegqgswzjvdrl4cr88vv9v75wjrssucaf2 --gas 10000000

junod tx wasm execute juno1hrpna9v7vs3stzyd4z3xf00676kf78zpe2u5ksvljswn2vnjp3ys7tlgu0 '{"bond": {}}' --from drip --amount 2000000000ujuno --gas 1000000
junod q wasm contract-state smart juno1suhgf5svhu4usrurvxzlgn54ksxmn8gljarjtxqnapv8kjnp4nrsf8smqw '{"balance": {"address": "juno1hj5fveer5cjtn4wd6wstzugjfdxzl0xps73ftl"}}'         

junod q wasm contract-state smart juno1hrpna9v7vs3stzyd4z3xf00676kf78zpe2u5ksvljswn2vnjp3ys7tlgu0 '{"validator_set":{}}' 

junod tx wasm execute juno1hrpna9v7vs3stzyd4z3xf00676kf78zpe2u5ksvljswn2vnjp3ys7tlgu0 '{"reinvest": {}}' --from drip --gas 1000000


junod tx wasm execute juno1suhgf5svhu4usrurvxzlgn54ksxmn8gljarjtxqnapv8kjnp4nrsf8smqw '{"send":{"amount": "1900000000" , "contract": "juno1hrpna9v7vs3stzyd4z3xf00676kf78zpe2u5ksvljswn2vnjp3ys7tlgu0", "msg": "eyJ1bmJvbmQiOiB7fX0="}}' --from drip --gas 1000000

junod q wasm contract-state smart juno1hrpna9v7vs3stzyd4z3xf00676kf78zpe2u5ksvljswn2vnjp3ys7tlgu0 '{"claims":{"address": "juno1hj5fveer5cjtn4wd6wstzugjfdxzl0xps73ftl"}}'         

junod tx wasm execute juno1hrpna9v7vs3stzyd4z3xf00676kf78zpe2u5ksvljswn2vnjp3ys7tlgu0 '{"claim":{}}' --from drip --gas 1000000

--- docker

junod tx staking unbond junovaloper1g8ejmp8yjjp99t5grgc3mvan5mlya8fnmn84s0 4999990000000ujuno --from validator --home /var/cosmos-chain/juno --keyring-backend test


// bonded 626F6E646564
// stake info 7374616B655F696E666F