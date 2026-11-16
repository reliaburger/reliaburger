@axis_vision @claim_integrated_platform @smoke
Feature: Integrated platform bootstrap
  As an operator without a dedicated platform team
  I want to deploy an application on a small cluster
  So that common platform services work without assembling external infrastructure

  @req_VIS-001 @claim_two_binaries_everything_inside @fresh_lab
  Scenario: Bootstrap and serve a multi-service application
    Given a disposable lab with 3 clean supported Linux nodes
    And a pinned Reliaburger build and its matching documentation
    When I provision a 3-node cluster using the documented procedure
    And I deploy the fixture application with 2 communicating services
    Then all 3 nodes should report healthy within the configured bootstrap timeout
    And the application should pass its external HTTP health probe
    And inter-service name resolution should pass its connectivity probe
    And the evidence should record external platform dependencies and manual interventions
