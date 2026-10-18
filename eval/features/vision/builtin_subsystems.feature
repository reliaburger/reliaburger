@axis_vision @claim_integrated_platform
Feature: Built-in platform services
  As an operator without a platform team
  I want the services a cluster needs to come with the binary
  So that I do not assemble and run a separate stack for each one

  @req_VIS-002
  Scenario Outline: The built-in <subsystem> works with nothing else installed
    Given a healthy disposable Reliaburger cluster
    And the native fixture application is running
    When I exercise the built-in <subsystem> through its documented interface
    Then it should give the documented result within the configured time

    @claim_secrets_age_encrypted_in_config
    Examples: secrets
      | case    | subsystem |
      | secrets | secrets   |

    @claim_pickle_registry_every_node
    Examples: registry
      | case     | subsystem |
      | registry | registry  |

    @claim_mayo_metrics_builtin
    Examples: metrics
      | case    | subsystem       |
      | metrics | metrics         |
      | top     | resource top    |

    @claim_ketchup_logs_builtin
    Examples: logs
      | case | subsystem |
      | logs | logs      |

    @claim_batch_jobs_one_lifecycle
    Examples: batch
      | case  | subsystem  |
      | batch | task array |

    @claim_brioche_dashboard_and_tui
    Examples: dashboard
      | case      | subsystem |
      | dashboard | dashboard |

    @claim_audit_log_durable_bounded
    Examples: audit
      | case  | subsystem |
      | audit | audit log |
