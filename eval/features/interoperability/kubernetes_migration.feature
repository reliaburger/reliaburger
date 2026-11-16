@axis_interoperability @claim_smooth_k8s_transition @smoke
Feature: Application migration from Kubernetes
  As an operator migrating a conventional application
  I want to reuse existing application manifests where supported
  So that migration requires limited rework and has predictable boundaries

  @req_INT-001 @claim_k8s_apply_converts_eleven_kinds @claim_k8s_drops_reported_not_silent
  Scenario: Import a baseline Kubernetes application
    Given a validated fixture application defined by Kubernetes Deployment, Service, ConfigMap, and Secret manifests
    And a healthy disposable Reliaburger cluster
    When I import the fixture using the documented Kubernetes migration path
    Then the harness should record accepted and rejected resource kinds
    And every rejected resource should have its diagnostic output preserved
    And the migrated application should pass the same external functional probes as the baseline
    And the evidence should record every manifest change and manual migration step
