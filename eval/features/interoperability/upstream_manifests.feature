@axis_interoperability @claim_smooth_k8s_transition
Feature: Unmodified upstream Kubernetes applications
  As an operator moving an existing application off Kubernetes
  I want its published manifests to run with few changes
  So that migration is a review, not a rewrite

  @req_INT-003 @claim_k8s_apply_converts_eleven_kinds @claim_k8s_drops_reported_not_silent
  Scenario Outline: Import <set> as published
    Given a healthy disposable Reliaburger cluster
    And the unmodified upstream manifests for <set>
    When I apply them with relish apply -f in their own namespace
    Then the migration report should account for every upstream resource
    And every imported workload should be running within the configured time
    When I add an ingress for the front service as the only manual change
    Then the front page should answer through that ingress within the configured time
    And the manual changes should not exceed the configured maximum

    Examples:
      | case           | set                         |
      | podinfo-webapp | podinfo 6.15.0 deploy/webapp |
      | online-boutique| Online Boutique v0.10.7     |

  @req_INT-004 @claim_k8s_import_export_roundtrip
  Scenario: A Reliaburger app exported to Kubernetes YAML runs on Kubernetes
    Given a validated fixture application defined by Kubernetes Deployment, Service, ConfigMap, and Secret manifests
    When I export the native fixture with relish export
    Then the export report should name what it could not express
    And the exported YAML should pass a server-side dry run on the Kubernetes baseline
    And the exported app should roll out on the Kubernetes baseline and answer by service name
