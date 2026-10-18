@axis_extensibility @claim_batteries_included
Feature: External controllers
  As an operator who would write a Kubernetes operator
  I want to drive the cluster from my own reconcile loop
  So that I can add behaviour the platform does not ship

  @req_EXT-006 @claim_event_and_log_streaming_apis @claim_cli_machine_readable_output
  Scenario: An external controller reconciles a custom spec over the API
    Given a healthy disposable Reliaburger cluster
    And an external controller that keeps a custom spec outside the cluster
    When the controller is given a spec for 2 replicas of a new app
    Then the app should run with 2 replicas within the configured convergence time
    And after a manual scale-down the controller should restore 2 replicas within the configured drift time
    And the cluster event API should list events for the app
