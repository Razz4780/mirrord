Add S3 bucket branching. `{"type": "s3", "source": {"params": {"bucket": "MY_BUCKET_ENV_VAR"}}}`
gives the session a branch bucket, cloned in the provider's cloud with no pod in the cluster,
seeded empty, with every object, or with the objects matching a list of regular expressions.
