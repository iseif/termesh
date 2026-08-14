# 1. Record architecture decisions

Date: 2026-07-29

## Status
Accepted

## Context
We want the reasoning behind significant, hard-to-reverse choices to be discoverable in the repo, not lost in chat or PR threads.

## Decision
We use Architecture Decision Records (Michael Nygard's format). Each load-bearing decision gets a numbered file here. Changes to the **action registry**, the **transaction spine**, or the **ACP client** require an ADR before implementation.

## Consequences
A lightweight paper trail; new contributors can read *why*, not just *what*. ADRs are immutable once Accepted — supersede with a new one rather than editing.
