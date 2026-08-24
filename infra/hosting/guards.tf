##############################################################################
# boxcode hosting guards
#
# Four layers, each catching what the one above it misses, each cheaper than
# the next. Nothing here is account-wide: this account is shared with 42 other
# Lambda functions and 8 EC2 instances, so every control is scoped to boxcode's
# own resources by name AND tag, and the IAM policy below is what makes that
# binding rather than merely intended.
#
#   L0  Shield Standard          free, automatic, absorbs L3/L4 at the edge
#   L1  WAF rate-based rules     blocks an L7 flood before anything is invoked
#   L2  API Gateway throttle     hard cap on invocations per second
#   L3  Reserved concurrency     hard cap on compute per app
#   L4  Budget alarm -> switch   last resort
#
# Deliberately NOT here, and never to be added:
#   - an account-level Lambda concurrency limit. It would throttle all 42
#     functions in this account into one shared pool. That is a company-wide
#     outage dressed as a cost control.
#   - an account-wide budget. Existing spend is orders of magnitude above any
#     threshold that would be useful for boxcode, so it would fire permanently
#     and be muted within a week.
##############################################################################

variable "region" {
  type    = string
  default = "us-east-1"
}

variable "app_prefix" {
  description = "Every hosted backend is named with this prefix. Must agree with APP_NAME_RE in kill-switch/scope.mjs."
  type        = string
  default     = "boxcode-app-"
}

variable "cost_tag_key" {
  description = "Cost-allocation tag the budget filters on. Must be activated in Billing > Cost allocation tags before the budget can see it."
  type        = string
  default     = "boxcode:hosting"
}

variable "budget_usd" {
  description = <<-EOT
    Monthly spend on tagged boxcode resources that trips the kill switch.

    Was 25 when hosted backends were Lambdas and the steady-state bill was
    pennies. The Firecracker host is a dedicated m8i.large tagged
    boxcode:hosting, so the baseline is now about $97/month and a threshold of
    25 would fire on the first day and every day after -- exactly the
    permanently-firing budget this file's header warns against, just scoped by
    tag instead of by account.

    110 is chosen to sit between the two numbers that matter: comfortably above
    the $96.81 estimate, so an ordinary month is silent, and below the $112.71
    worst realistic overshoot, so a month that combines ten times the expected
    traffic with a badly-behaved log producer trips it. The point is to be told
    before the invoice, not after.
  EOT
  type        = string
  default     = "110"
}

variable "app_concurrency" {
  description = "Reserved concurrency per hosted backend. Also what the kill switch restores to."
  type        = number
  default     = 2
}

variable "alert_email" {
  type = string
}

##############################################################################
# L4 -- the kill switch, and the IAM policy that is the actual guarantee
##############################################################################

# This is the important resource in the file.
#
# scope.mjs decides which functions the kill switch *intends* to touch. This
# policy decides which ones it *can*. Code can have a bug; an IAM resource
# constraint is enforced by AWS before the call runs, so even a kill switch
# that tried to throttle gpurouter-agent would be refused by the API.
#
# If you change the naming scheme, change it in three places or the switch
# silently stops working: here, APP_NAME_RE in scope.mjs, and whatever names
# deploy-control creates.
data "aws_iam_policy_document" "kill_switch" {
  # Reading is unavoidably account-wide: ListFunctions has no resource-level
  # permission in Lambda's IAM model. That is acceptable -- listing changes
  # nothing, and scope.mjs filters what comes back.
  statement {
    sid       = "ReadOnlyListing"
    actions   = ["lambda:ListFunctions", "lambda:ListTags", "lambda:GetFunction"]
    resources = ["*"]
  }

  # Writing is not. This is the line that protects the other 42 functions.
  statement {
    sid = "ThrottleOnlyBoxcodeApps"
    actions = [
      "lambda:PutFunctionConcurrency",
      "lambda:DeleteFunctionConcurrency",
    ]
    resources = [
      "arn:aws:lambda:${var.region}:${data.aws_caller_identity.current.account_id}:function:${var.app_prefix}*",
    ]
  }

  statement {
    sid       = "OwnLogs"
    actions   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["arn:aws:logs:${var.region}:${data.aws_caller_identity.current.account_id}:log-group:/aws/lambda/boxcode-kill-switch:*"]
  }
}

data "aws_caller_identity" "current" {}

resource "aws_iam_role" "kill_switch" {
  name = "boxcode-kill-switch"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy" "kill_switch" {
  role   = aws_iam_role.kill_switch.id
  policy = data.aws_iam_policy_document.kill_switch.json
}

resource "aws_lambda_function" "kill_switch" {
  function_name = "boxcode-kill-switch"
  role          = aws_iam_role.kill_switch.arn
  runtime       = "nodejs22.x"
  handler       = "index.handler"
  filename      = "kill-switch.zip"
  timeout       = 60
  # Never given reserved concurrency of its own: it must be able to run during
  # exactly the incident where everything else is throttled.
  environment {
    variables = { APP_CONCURRENCY = tostring(var.app_concurrency) }
  }
  tags = { (var.cost_tag_key) = "true" }
}

##############################################################################
# The budget that fires it -- filtered by tag, never account-wide
##############################################################################

resource "aws_sns_topic" "alerts" {
  name = "boxcode-hosting-alerts"
}

resource "aws_sns_topic_subscription" "email" {
  topic_arn = aws_sns_topic.alerts.arn
  protocol  = "email"
  endpoint  = var.alert_email
}

resource "aws_sns_topic_subscription" "kill_switch" {
  topic_arn = aws_sns_topic.alerts.arn
  protocol  = "lambda"
  endpoint  = aws_lambda_function.kill_switch.arn
}

resource "aws_lambda_permission" "sns_invoke" {
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.kill_switch.function_name
  principal     = "sns.amazonaws.com"
  source_arn    = aws_sns_topic.alerts.arn
}

resource "aws_budgets_budget" "hosting" {
  name         = "boxcode-hosting"
  budget_type  = "COST"
  limit_amount = var.budget_usd
  limit_unit   = "USD"
  time_unit    = "MONTHLY"

  # The whole point. Without this filter the budget measures the account --
  # which already spends far more than any boxcode threshold, so it would fire
  # immediately, permanently, and be ignored.
  cost_filter {
    name   = "TagKeyValue"
    values = ["user:${var.cost_tag_key}$true"]
  }

  # Warn well before acting, so a human can look before the switch trips.
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 60
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.alert_email]
  }

  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 100
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_sns_topic_arns  = [aws_sns_topic.alerts.arn]
    subscriber_email_addresses = [var.alert_email]
  }
}

##############################################################################
# Detection -- the four signals worth waking someone for
##############################################################################

# Account-wide concurrency, watched but never *capped*. If boxcode's apps are
# eating the pool the other 42 functions share, that is the thing to know
# before anyone else notices.
resource "aws_cloudwatch_metric_alarm" "account_concurrency" {
  alarm_name          = "boxcode-account-concurrency-high"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "ConcurrentExecutions"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Maximum"
  threshold           = 400
  alarm_description   = "Lambda concurrency across this shared account is high. Watch only -- never cap this account-wide."
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "app_throttles" {
  alarm_name          = "boxcode-app-throttles"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Throttles"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 100
  alarm_description   = "Hosted apps are being throttled -- reserved concurrency is doing its job, but something is pushing hard."
  alarm_actions       = [aws_sns_topic.alerts.arn]
}

##############################################################################
# L1 -- WAF. Blocks at the edge, before API Gateway or Lambda is reached, so a
# blocked request costs WAF's per-request fee instead of an invocation.
#
# Attached to the CloudFront distribution, so it must be created in us-east-1.
##############################################################################

resource "aws_wafv2_web_acl" "hosting" {
  name  = "boxcode-hosting"
  scope = "CLOUDFRONT"

  default_action {
    allow {}
  }

  # The blunt instrument: any one address asking for more than this in five
  # minutes is blocked. Set well above what a real visitor to a demo page
  # generates, and well below what a flood does.
  rule {
    name     = "rate-per-ip"
    priority = 1
    action {
      block {}
    }
    statement {
      rate_based_statement {
        limit              = 2000
        aggregate_key_type = "IP"
      }
    }
    visibility_config {
      cloudwatch_metrics_enabled = true
      metric_name                = "rate-per-ip"
      sampled_requests_enabled   = true
    }
  }

  # Tighter still on the paths that create or query things, as opposed to the
  # ones that serve static files.
  rule {
    name     = "rate-per-ip-api"
    priority = 2
    action {
      block {}
    }
    statement {
      rate_based_statement {
        limit              = 300
        aggregate_key_type = "IP"
        scope_down_statement {
          byte_match_statement {
            search_string         = "/api/"
            positional_constraint = "STARTS_WITH"
            field_to_match {
              uri_path {}
            }
            text_transformation {
              priority = 0
              type     = "NONE"
            }
          }
        }
      }
    }
    visibility_config {
      cloudwatch_metrics_enabled = true
      metric_name                = "rate-per-ip-api"
      sampled_requests_enabled   = true
    }
  }

  rule {
    name     = "amazon-ip-reputation"
    priority = 3
    override_action {
      none {}
    }
    statement {
      managed_rule_group_statement {
        vendor_name = "AWS"
        name        = "AWSManagedRulesAmazonIpReputationList"
      }
    }
    visibility_config {
      cloudwatch_metrics_enabled = true
      metric_name                = "amazon-ip-reputation"
      sampled_requests_enabled   = true
    }
  }

  visibility_config {
    cloudwatch_metrics_enabled = true
    metric_name                = "boxcode-hosting"
    sampled_requests_enabled   = true
  }

  tags = { (var.cost_tag_key) = "true" }
}

##############################################################################
# L2 -- API Gateway throttling.
#
# Costs nothing and is the hard cap on how fast Lambda can be invoked at all.
# WAF stops one address flooding; this stops the aggregate, including a
# distributed flood where no single address trips a per-IP rule.
#
# Uncomment and wire to the HTTP API once the hosting stack creates it.
##############################################################################

# resource "aws_apigatewayv2_stage" "hosting" {
#   api_id      = aws_apigatewayv2_api.hosting.id
#   name        = "$default"
#   auto_deploy = true
#   default_route_settings {
#     throttling_rate_limit  = 200   # steady-state requests/second, all apps
#     throttling_burst_limit = 400   # bucket depth
#   }
# }

output "kill_switch_arn" {
  value = aws_lambda_function.kill_switch.arn
}

output "alerts_topic_arn" {
  value = aws_sns_topic.alerts.arn
}
