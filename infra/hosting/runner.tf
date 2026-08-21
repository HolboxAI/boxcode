##############################################################################
# The runner box -- where hosted full-stack backends actually run.
#
# One instance, on spot, with an on-demand fallback. Spot is ~$40/month against
# ~$87 on-demand and that difference is the reason this file is shaped the way
# it is; everything below is either buying that discount or paying for it.
#
# The three things that are load-bearing and non-obvious:
#
#   1. ONE availability zone. The data volume holds the project registry and
#      Postgres, and an EBS volume can only attach inside its own AZ. Spreading
#      the ASG across zones would let it launch a replacement that cannot reach
#      any of the state it is replacing.
#
#   2. min = max = desired = 1. The same single volume forbids two instances,
#      which also means no launch-before-terminate and therefore no useful
#      capacity rebalancing -- see the comment on the ASG.
#
#   3. The on-demand percentage is deliberately NOT managed by Terraform after
#      creation. The spot-fallback function owns it at runtime, and an apply
#      that reset it would undo the fallback during exactly the outage it
#      exists for. See the lifecycle block.
#
# Untrusted code runs on this box. It gets its own security group, its own
# instance profile, and shares nothing with boxcode-auth -- which runs its
# control-plane as root with Postgres on `trust`, and must never host a
# stranger's container.
##############################################################################

variable "runner_az" {
  description = "The one availability zone the runner and its data volume live in. Pinned because EBS cannot cross zones."
  type        = string
  default     = "us-east-1a"
}

variable "runner_subnet_id" {
  description = "Subnet in runner_az. Must be in the same VPC as the boxcode-auth box so the two can talk privately."
  type        = string
}

variable "runner_vpc_id" {
  type = string
}

variable "runner_instance_types" {
  description = <<-EOT
    Candidate types, all 2 vCPU / 8 GiB so capacity is identical whichever one
    the spot allocator picks. Diversity here is the primary defence against
    interruption: a shortage usually hits one type in one zone, and with seven
    to choose from the group almost always finds capacity without needing the
    fallback at all. x86_64 only -- Graviton was rejected because some container
    images and npm prebuilds have no ARM build.
  EOT
  type        = list(string)
  default     = ["t3.large", "t3a.large", "m5.large", "m5a.large", "m6i.large", "m6a.large", "m5n.large"]
}

variable "runner_data_volume_gb" {
  description = "Registry, Postgres, per-app state, cached builds. Survives instance replacement; that is the whole point of it being separate."
  type        = number
  default     = 100
}

variable "runner_bootstrap_bucket" {
  description = "S3 bucket holding the infra/hosting bundle that user_data fetches on boot. Boot must be unattended -- an ASG replaces instances at 3am."
  type        = string
}

variable "admin_cidr" {
  description = "Source allowed to reach SSH. Empty list closes port 22 entirely, which is the right answer once SSM Session Manager is proven working."
  type        = list(string)
  default     = []
}

locals {
  runner_name = "boxcode-runner"
  runner_tags = {
    Name               = "boxcode-runner"
    (var.cost_tag_key) = "true"
  }
}

##############################################################################
# Image and edge prefix list
##############################################################################

data "aws_ssm_parameter" "al2023" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64"
}

# CloudFront's own egress ranges, maintained by AWS. Using this instead of
# 0.0.0.0/0 means the box only ever answers the CDN -- someone who resolves
# apps.boxcode.sh and hits the origin directly gets nothing, so WAF and the
# rate limits cannot be walked around by skipping the edge.
data "aws_ec2_managed_prefix_list" "cloudfront" {
  name = "com.amazonaws.global.cloudfront.origin-facing"
}

##############################################################################
# Security group
##############################################################################

resource "aws_security_group" "runner" {
  name        = "boxcode-runner"
  description = "boxcode hosted backends. Answers CloudFront only."
  vpc_id      = var.runner_vpc_id
  tags        = local.runner_tags
}

resource "aws_vpc_security_group_ingress_rule" "runner_https" {
  security_group_id = aws_security_group.runner.id
  prefix_list_id    = data.aws_ec2_managed_prefix_list.cloudfront.id
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
  description       = "HTTPS from CloudFront only"
}

# Port 80 is not for traffic -- CloudFront is told to speak HTTPS to the origin.
# It exists because certbot's HTTP-01 challenge needs it, and Let's Encrypt
# validates from its own addresses, not CloudFront's.
resource "aws_vpc_security_group_ingress_rule" "runner_acme" {
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "tcp"
  from_port         = 80
  to_port           = 80
  description       = "HTTP for the Let's Encrypt HTTP-01 challenge"
}

resource "aws_vpc_security_group_ingress_rule" "runner_ssh" {
  for_each          = toset(var.admin_cidr)
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = each.value
  ip_protocol       = "tcp"
  from_port         = 22
  to_port           = 22
  description       = "SSH from an administrator"
}

# The HOST needs egress -- to pull base images, install packages, reach S3 and
# SSM, and let the build sandbox run npm install. Hosted app containers do NOT,
# and their containment is not done here: they sit on a Docker bridge created
# --internal, which has no default route at all, backed by an explicit nftables
# drop. A security group cannot express "this process may reach the internet and
# that one may not" on a single host, so it does not try.
resource "aws_vpc_security_group_egress_rule" "runner_out" {
  security_group_id = aws_security_group.runner.id
  cidr_ipv4         = "0.0.0.0/0"
  ip_protocol       = "-1"
  description       = "Host egress. App containers are confined on the box, not here."
}

##############################################################################
# The data volume -- the reason the AZ is pinned
##############################################################################

resource "aws_ebs_volume" "runner_data" {
  availability_zone = var.runner_az
  size              = var.runner_data_volume_gb
  type              = "gp3"
  encrypted         = true
  tags              = merge(local.runner_tags, { Name = "boxcode-runner-data" })

  # Losing this loses every live project's registry entry and database. Nothing
  # in this stack should ever replace it, and an accidental `terraform apply`
  # that wanted to is a bug worth failing on rather than discovering afterwards.
  lifecycle {
    prevent_destroy = true
  }
}

##############################################################################
# Instance role -- scoped so a compromised box cannot become an account problem
##############################################################################

data "aws_iam_policy_document" "runner_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "runner" {
  name               = "boxcode-runner"
  assume_role_policy = data.aws_iam_policy_document.runner_assume.json
  tags               = local.runner_tags
}

# Session Manager, which is also how the kill switch reaches this box. Included
# deliberately: it means port 22 can be closed entirely, and it means stopping
# every hosted container during an incident does not depend on SSH keys.
resource "aws_iam_role_policy_attachment" "runner_ssm" {
  role       = aws_iam_role.runner.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

data "aws_iam_policy_document" "runner" {
  # Attaching its own data volume on boot. Describe cannot be resource-scoped in
  # EC2's IAM model; Attach can, and is -- to this one volume, and only to
  # instances carrying the hosting tag. A box that escapes its containment still
  # cannot pick up somebody else's disk.
  statement {
    sid       = "DescribeForBoot"
    actions   = ["ec2:DescribeVolumes", "ec2:DescribeTags"]
    resources = ["*"]
  }

  statement {
    sid       = "AttachOwnDataVolume"
    actions   = ["ec2:AttachVolume"]
    resources = [aws_ebs_volume.runner_data.arn]
  }

  statement {
    sid       = "AttachOnlyToTaggedInstances"
    actions   = ["ec2:AttachVolume"]
    resources = ["arn:aws:ec2:${var.region}:${data.aws_caller_identity.current.account_id}:instance/*"]
    condition {
      test     = "StringEquals"
      variable = "aws:ResourceTag/${var.cost_tag_key}"
      values   = ["true"]
    }
  }

  # The bootstrap bundle, and nothing else in the bucket.
  statement {
    sid       = "ReadOwnBootstrap"
    actions   = ["s3:GetObject"]
    resources = ["arn:aws:s3:::${var.runner_bootstrap_bucket}/hosting/*"]
  }

  statement {
    sid       = "OwnLogs"
    actions   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["arn:aws:logs:${var.region}:${data.aws_caller_identity.current.account_id}:log-group:/boxcode/runner:*"]
  }
}

resource "aws_iam_role_policy" "runner" {
  role   = aws_iam_role.runner.id
  policy = data.aws_iam_policy_document.runner.json
}

resource "aws_iam_instance_profile" "runner" {
  name = "boxcode-runner"
  role = aws_iam_role.runner.name
}

##############################################################################
# Launch template
##############################################################################

resource "aws_launch_template" "runner" {
  name          = "boxcode-runner"
  image_id      = data.aws_ssm_parameter.al2023.value
  instance_type = var.runner_instance_types[0] # overridden per-type by the ASG

  iam_instance_profile {
    arn = aws_iam_instance_profile.runner.arn
  }

  vpc_security_group_ids = [aws_security_group.runner.id]

  block_device_mappings {
    device_name = "/dev/xvda"
    ebs {
      volume_size           = 30
      volume_type           = "gp3"
      encrypted             = true
      delete_on_termination = true
    }
  }

  metadata_options {
    http_tokens                 = "required" # IMDSv2 only
    http_put_response_hop_limit = 1          # containers cannot reach the metadata service
  }

  # hop_limit = 1 above is quietly one of the more important lines in this file:
  # it stops a container on the Docker bridge from reaching 169.254.169.254 and
  # reading this instance's role credentials. Docker adds a hop, so a limit of 1
  # reaches the host and nothing else.

  user_data = base64encode(templatefile("${path.module}/user-data.sh.tftpl", {
    data_volume_id = aws_ebs_volume.runner_data.id
    bucket         = var.runner_bootstrap_bucket
    region         = var.region
  }))

  tag_specifications {
    resource_type = "instance"
    tags          = local.runner_tags
  }
  tag_specifications {
    resource_type = "volume"
    tags          = local.runner_tags
  }

  tags = local.runner_tags
}

##############################################################################
# The Auto Scaling group -- spot, with the fallback the function below drives
##############################################################################

resource "aws_autoscaling_group" "runner" {
  name             = local.runner_name
  desired_capacity = 1
  min_size         = 1
  max_size         = 1

  # One subnet, on purpose. See the header: the data volume pins the AZ.
  vpc_zone_identifier = [var.runner_subnet_id]

  health_check_type = "EC2"
  # Boot installs Docker, gVisor, nginx and Postgres, mounts the data volume and
  # restores every live app from the registry. Marking it unhealthy before that
  # finishes would put the group into a replace loop that never converges.
  health_check_grace_period = 900

  # Capacity rebalancing is deliberately OFF. It works by launching a
  # replacement *before* terminating the instance at risk, which needs
  # max_size >= 2 -- and two instances cannot share one EBS data volume. With
  # max_size = 1 it can do nothing useful, so enabling it would be cargo cult.
  capacity_rebalance = false

  mixed_instances_policy {
    instances_distribution {
      on_demand_base_capacity = 0
      # Spot. The spot-fallback function raises this to 100 when spot capacity
      # runs out and lowers it again once the shortage passes.
      on_demand_percentage_above_base_capacity = 0
      spot_allocation_strategy                 = "price-capacity-optimized"
    }

    launch_template {
      launch_template_specification {
        launch_template_id = aws_launch_template.runner.id
        version            = "$Latest"
      }

      dynamic "override" {
        for_each = var.runner_instance_types
        content {
          instance_type = override.value
        }
      }
    }
  }

  dynamic "tag" {
    for_each = local.runner_tags
    content {
      key                 = tag.key
      value               = tag.value
      propagate_at_launch = true
    }
  }

  # The fallback owns the on-demand percentage at runtime. Without this, an
  # unrelated `terraform apply` during a capacity shortage would reset the group
  # to spot-only and take the platform back down -- an outage caused by the
  # thing that was supposed to prevent it.
  #
  # boxcode:fallback-since is also written at runtime, by the same function.
  lifecycle {
    ignore_changes = [
      mixed_instances_policy[0].instances_distribution[0].on_demand_percentage_above_base_capacity,
      tag,
    ]
  }
}

##############################################################################
# The fallback function
##############################################################################

data "aws_iam_policy_document" "spot_fallback" {
  statement {
    sid       = "ReadTheGroup"
    actions   = ["autoscaling:DescribeAutoScalingGroups"]
    resources = ["*"] # no resource-level permission exists for this action
  }

  # The guarantee. Even a bug in policy.mjs cannot reconfigure another group in
  # this shared account, because AWS refuses the call before the code runs.
  statement {
    sid = "ReconfigureOnlyTheRunnerGroup"
    actions = [
      "autoscaling:UpdateAutoScalingGroup",
      "autoscaling:CreateOrUpdateTags",
    ]
    resources = [aws_autoscaling_group.runner.arn]
  }

  statement {
    sid       = "OwnLogs"
    actions   = ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"]
    resources = ["arn:aws:logs:${var.region}:${data.aws_caller_identity.current.account_id}:log-group:/aws/lambda/boxcode-spot-fallback:*"]
  }
}

resource "aws_iam_role" "spot_fallback" {
  name = "boxcode-spot-fallback"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "lambda.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
  tags = local.runner_tags
}

resource "aws_iam_role_policy" "spot_fallback" {
  role   = aws_iam_role.spot_fallback.id
  policy = data.aws_iam_policy_document.spot_fallback.json
}

resource "aws_lambda_function" "spot_fallback" {
  function_name = "boxcode-spot-fallback"
  role          = aws_iam_role.spot_fallback.arn
  runtime       = "nodejs22.x"
  handler       = "index.handler"
  filename      = "spot-fallback.zip"
  timeout       = 30
  # Never given reserved concurrency: like the kill switch, it has to be able to
  # run during exactly the incident where everything else is constrained.
  tags = local.runner_tags
}

resource "aws_cloudwatch_event_rule" "launch_failed" {
  name        = "boxcode-runner-launch-failed"
  description = "A spot launch for the runner group failed -- probably no capacity."
  event_pattern = jsonencode({
    source        = ["aws.autoscaling"]
    "detail-type" = ["EC2 Instance Launch Unsuccessful"]
    detail = {
      AutoScalingGroupName = [local.runner_name]
    }
  })
  tags = local.runner_tags
}

resource "aws_cloudwatch_event_rule" "fallback_sweep" {
  name                = "boxcode-runner-fallback-sweep"
  description         = "Hourly. The only thing that ever moves the group back toward spot."
  schedule_expression = "rate(1 hour)"
  tags                = local.runner_tags
}

resource "aws_cloudwatch_event_target" "launch_failed" {
  rule      = aws_cloudwatch_event_rule.launch_failed.name
  target_id = "spot-fallback"
  arn       = aws_lambda_function.spot_fallback.arn
}

resource "aws_cloudwatch_event_target" "fallback_sweep" {
  rule      = aws_cloudwatch_event_rule.fallback_sweep.name
  target_id = "spot-fallback"
  arn       = aws_lambda_function.spot_fallback.arn
}

resource "aws_lambda_permission" "launch_failed" {
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.spot_fallback.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.launch_failed.arn
}

resource "aws_lambda_permission" "fallback_sweep" {
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.spot_fallback.function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.fallback_sweep.arn
}

##############################################################################
# The alarms that matter for a single box
##############################################################################

# Nothing in service means every hosted app is down. This is the one alarm on
# this stack worth waking someone for.
resource "aws_cloudwatch_metric_alarm" "runner_down" {
  alarm_name          = "boxcode-runner-down"
  comparison_operator = "LessThanThreshold"
  evaluation_periods  = 2
  metric_name         = "GroupInServiceInstances"
  namespace           = "AWS/AutoScaling"
  period              = 60
  statistic           = "Minimum"
  threshold           = 1
  dimensions          = { AutoScalingGroupName = local.runner_name }
  # Missing data is not "fine" here -- an ASG that stopped reporting is at least
  # as concerning as one reporting zero.
  treat_missing_data = "breaching"
  alarm_description  = "No runner instance in service. Every hosted app is down."
  alarm_actions      = [aws_sns_topic.alerts.arn]
  ok_actions         = [aws_sns_topic.alerts.arn]
  tags               = local.runner_tags
}

output "runner_asg_name" {
  value = aws_autoscaling_group.runner.name
}

output "runner_data_volume_id" {
  value = aws_ebs_volume.runner_data.id
}

output "runner_security_group_id" {
  value = aws_security_group.runner.id
}
