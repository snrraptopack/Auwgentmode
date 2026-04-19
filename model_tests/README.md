This is a live api test

# All scenarios, default model
cargo run -p model_tests

# Specific model and scenario
cargo run -p model_tests -- --model llama-3.3-70b-versatile --scenario parallel

# Orphan stress test (3 rounds)
cargo run -p model_tests -- --scenario orphan --rounds 3

# See the Lua the model wrote
cargo run -p model_tests -- --verbose
