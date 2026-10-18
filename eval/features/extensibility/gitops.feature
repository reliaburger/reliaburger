@axis_extensibility @claim_batteries_included @reconfigures
Feature: GitOps from an external repository
  As an operator who keeps desired state in Git
  I want the cluster to follow a repository I host
  So that changes go through review instead of ad hoc applies

  @req_EXT-008 @claim_lettuce_gitops_builtin
  Scenario: The cluster follows commits, reverts drift and honours deletes
    Given a healthy disposable Reliaburger cluster
    And the native fixture application is running
    And a git repository served over HTTP from the lab gateway
    And every node syncs that repository
    When I commit an app with 2 replicas to the repository
    Then the app should run with 2 replicas within the configured sync time
    And apps applied outside GitOps should be left alone
    When I scale the GitOps app to 1 replica by hand
    Then the next sync should restore 2 replicas within the configured drift time
    When I delete the app from the repository
    Then the app should be removed within the configured delete time
