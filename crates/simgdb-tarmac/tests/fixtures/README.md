# Fixtures

`ttu_parsertest.txt` and `ttu_parsertest.ref` are copied unchanged from
[ARM-software/tarmac-trace-utilities](https://github.com/ARM-software/tarmac-trace-utilities)
(`tests/parsertest.txt`, `tests/parsertest.ref`, commit of 2026-09-03), Copyright Arm Limited,
Apache License 2.0. The `.txt` lines were collected from real trace generators (Fast Models, Cortex-M3/M4/R5/A53
RTL, Cycle Models, Neoverse testbenches); the `.ref` file is that project's parse of each line.
SimGDB's parser is tested against those expected results as an independent implementation.
