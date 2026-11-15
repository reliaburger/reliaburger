# Reject invalid autoscale targets (#554)

An accepted `0%`, negative or nonfinite target silently prevents meaningful
scaling. Add failing configuration and evaluation tests, require a positive
finite target, ignore nonfinite measurements, and explain the control boundary
in chapter 9. Keep finite utilisation targets above 100% valid because requests
aren't limits. Run portable CI; no protocol or durable format change is needed.
