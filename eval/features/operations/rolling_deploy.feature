@axis_operations @claim_self_healing
Feature: Rolling deploys and automatic rollback
  As an operator shipping changes to a running service
  I want deploys to replace instances behind a health gate
  So that a good change causes no downtime and a bad one undoes itself

  @req_OPS-010 @claim_rolling_deploys_health_gated_auto_rollback
  Scenario: A healthy change rolls out without downtime
    Given a healthy disposable Reliaburger cluster
    And the native fixture application is running
    And an external probe recording availability once per second
    When I apply a changed frontend message
    Then every ingress should serve the new message within the configured completion time
    And availability during the rollout should meet the configured threshold

  @req_OPS-011 @claim_rolling_deploys_health_gated_auto_rollback
  Scenario: A change whose health check never passes is rolled back
    Given a healthy disposable Reliaburger cluster
    And the native fixture application is running
    And an external probe recording availability once per second
    When I apply a frontend version whose health check never passes
    Then the deploy should be rolled back within the configured rollback time
    And the previous version should keep serving on every ingress
    And availability during the failed rollout should meet the configured threshold
