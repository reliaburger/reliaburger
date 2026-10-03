# Bound cron arithmetic before incrementing (#541)

`59/255 * * * *` panics in a debug build and wraps into every minute in
a release build. Keep the field iteration wide enough to represent the
next value, and stop at the field's upper bound.

1. Add a failing regression for maximal steps on single values, ranges and
   wildcards.
2. Widen the iteration arithmetic while preserving the stored field type.
3. Reject malformed schedules during configuration admission, before the agent loop sees them.
4. Explain the boundary in chapter 8 and run portable CI.

This changes neither wire data nor persisted schedules. Existing expressions
retain their mathematical meaning, including steps larger than a field.
