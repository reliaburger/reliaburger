@axis_operations @claim_integrated_platform
Feature: API token roles and scopes
  As an operator handing cluster access to people and CI jobs
  I want each token limited to its role and namespaces
  So that a leaked deploy token cannot take over the cluster

  @req_OPS-003 @claim_token_roles_and_scoped_rbac
  Scenario Outline: A <role> token scoped to <scope> tries to <action>
    Given a healthy disposable Reliaburger cluster
    And the namespace rbeval-rbac exists
    And a <role> API token scoped to <scope>
    When the token is used to <action>
    Then the API should <verdict> the request
    And a refused request should appear in the audit log as api.refused

    Examples:
      | case                 | role      | scope        | action                       | verdict |
      | readonly-status      | read-only | all          | read cluster status          | allow   |
      | readonly-apply       | read-only | all          | apply an app in rbeval-rbac  | refuse  |
      | deployer-apply-own   | deployer  | rbeval-rbac  | apply an app in rbeval-rbac  | allow   |
      | deployer-apply-other | deployer  | rbeval-rbac  | apply an app in default      | refuse  |
      | deployer-see-other   | deployer  | rbeval-rbac  | inspect an app in default    | refuse  |
      | deployer-tokens      | deployer  | all          | create an API token          | refuse  |
      | deployer-namespace   | deployer  | all          | apply a namespace block      | refuse  |
