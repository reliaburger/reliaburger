@axis_extensibility @claim_batteries_included @reconfigures
Feature: Outbound alert webhooks
  As an operator with an existing paging or chat tool
  I want the cluster's alerts delivered to my own endpoint
  So that on-call does not have to watch the built-in dashboard

  @req_EXT-003 @claim_alert_webhooks_outbound
  Scenario: A failed job run raises one signed webhook alert
    Given a healthy disposable Reliaburger cluster
    And an alert receiver on the lab gateway
    And every node sends alerts to that receiver with a signing secret
    When I run a task array whose only task fails
    Then the receiver should get a job_run_failed alert within the configured delivery time
    And the alert body should carry the documented fields
    And the X-Mayo-Signature-256 header should be the HMAC-SHA256 of the body
    And only one job_run_failed alert should arrive for the run
