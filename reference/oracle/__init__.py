"""The independent Python differential oracle (Task 14).

Deliberately slow, obviously correct, independently derived (plan §10.3). It re-implements the
*definitions* — quantisation (R2), priority (R3), mask = set union from ``pairs.parquet``, count
= brute-force loop — from scratch. It must never call into the Rust engine or share logic with
it; it only reads bundle files directly and talks to a running server over HTTP (Python is a
consumer, never a component — CLAUDE.md "Working method").
"""
