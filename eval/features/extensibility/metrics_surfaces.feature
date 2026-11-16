@axis_extensibility @claim_batteries_included
Feature: Metrics for an external monitoring stack
  As an operator who already runs Prometheus
  I want to collect cluster and application metrics from outside
  So that the cluster fits the monitoring I have

  @req_EXT-004 @claim_no_prometheus_endpoint_remote_read_or_promql @claim_app_metrics_scraping_and_extra_targets
  Scenario Outline: Collect metrics through <surface>
    Given a healthy disposable Reliaburger cluster
    And the native fixture application is running
    When I request metrics through <surface> from the workstation
    Then the response should be in the Prometheus text exposition format

    Examples:
      | case        | surface                                  |
      | node-api    | the node API path /metrics               |
      | remote-read | the node API path /v1/read               |
      | app-ingress | the application's own ingress at /metrics |
