#!/bin/sh
# Halt a wedged board over the built-in USB-JTAG and say where both cores are stuck.
#
# Run this while the device is still frozen and still powered. It halts the CPUs,
# which is destructive to the hang state only in the sense that you cannot resume
# and expect the timing back — the program counters are read first.
#
# Needs Espressif's OpenOCD fork (openocd-esp32). Vanilla OpenOCD 0.12 carries the
# esp_usb_jtag driver but fails at "could not retrieve jtag_caps descriptor".
set -eu

ELF=${ELF:-/home/paul/projects/medienzeit/firmware/target/xtensa-esp32s3-none-elf/release/medienzeit-firmware}
OOCD=${OOCD:-openocd}
OUT=${OUT:-/home/paul/projects/medienzeit/postmortem.txt}
ADDR2LINE=/home/paul/.rustup/toolchains/esp/xtensa-esp-elf/esp-15.2.0_20250920/xtensa-esp-elf/bin/xtensa-esp32s3-elf-addr2line

echo "== halting both cores ==" | tee "$OUT"
"$OOCD" -f board/esp32s3-builtin.cfg \
    -c "init" \
    -c "halt" \
    -c "echo {--- core 0 ---}" \
    -c "targets esp32s3.cpu0" \
    -c "reg pc" \
    -c "reg a0" \
    -c "reg a1" \
    -c "echo {--- core 1 ---}" \
    -c "targets esp32s3.cpu1" \
    -c "reg pc" \
    -c "reg a0" \
    -c "reg a1" \
    -c "echo {--- backtraces ---}" \
    -c "targets esp32s3.cpu0" -c "bt" \
    -c "targets esp32s3.cpu1" -c "bt" \
    -c "exit" 2>&1 | tee -a "$OUT"

echo
echo "== symbolising every address seen ==" | tee -a "$OUT"
grep -oE '0x4[0-9a-f]{7}' "$OUT" | sort -u | while read -r a; do
    printf '%s  ' "$a"
    "$ADDR2LINE" -e "$ELF" -f -C -i "$a" 2>/dev/null | tr '\n' ' '
    echo
done | tee -a "$OUT"

echo
echo "written to $OUT"
