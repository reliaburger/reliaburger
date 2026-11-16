@axis_interoperability @claim_k8s_apply_converts_eleven_kinds @claim_k8s_drops_reported_not_silent
Feature: Kubernetes kind coverage of the importer
  As an operator assessing a migration
  I want to know which Kubernetes kinds convert and which are dropped
  So that I can predict the rework before I start

  @req_INT-002 @offline
  Scenario Outline: Import a <case> manifest
    Given the single-kind Kubernetes manifest "<case>"
    When I convert it with relish import
    Then the importer outcome should be "<outcome>"
    And the converted configuration should contain "<evidence>"
    And no resource in the manifest should be silently lost

    Examples: Documented as converted
      | case                    | outcome   | evidence                  |
      | Deployment              | converted | [app.web]                 |
      | StatefulSet             | converted | [app.db]                  |
      | DaemonSet               | converted | replicas = "*"            |
      | Service                 | converted | port = 9898               |
      | Ingress                 | converted | host = "web.kinds.test"   |
      | HorizontalPodAutoscaler | converted | [app.web.autoscale]       |
      | Job                     | converted | [job.migrate]             |
      | CronJob                 | converted | schedule = "*/5 * * * *"  |
      | Namespace               | converted | [namespace.shop]          |

    Examples: Documented as not wired into workloads
      | case      | outcome  | evidence |
      | ConfigMap | reported | -        |
      | Secret    | reported | -        |

    Examples: Documented as dropped
      | case                     | outcome | evidence |
      | NetworkPolicy            | dropped | -        |
      | PersistentVolumeClaim    | dropped | -        |
      | CustomResourceDefinition | dropped | -        |
      | CustomResource           | dropped | -        |
      | ServiceAccount           | dropped | -        |
      | Role                     | dropped | -        |
