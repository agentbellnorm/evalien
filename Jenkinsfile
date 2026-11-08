// Builds the image, pushes it to the cluster registry, and points the
// deployment at it. ArgoCD deploys from deploy/k8s when the tag changes.
pipeline {
    environment {
        REGISTRY_URL = 'docker-registry-service.docker-registry.svc.cluster.local:5000'
        REGISTRY_CREDS = credentials('docker-registry-private')
        IMAGE_NAME = 'evalien'
        REPO_SLUG = 'agentbellnorm/evalien'
        GIT_SHORT_HASH = "${GIT_COMMIT[0..7]}"
        DEPLOYMENT_MANIFEST = 'deploy/k8s/04-deployment.yaml'
    }
    agent {
        kubernetes {
            agentContainer 'jnlp'
            yaml """
            apiVersion: v1
            kind: Pod
            spec:
              # The Rust build needs a few GB of memory: build on the Mac minis.
              nodeSelector:
                type: m
              securityContext:
                runAsUser: 1000
              containers:
              - name: yq
                image: mikefarah/yq:4.44.1
                command:
                  - /bin/sh
                tty: true
              - name: podman
                image: quay.io/containers/podman:v5.2.3
                tty: true
                securityContext:
                  privileged: true  # Required for using the overlay driver
                env:
                  - name: STORAGE_DRIVER
                    value: overlay
                resources:
                  requests:
                    memory: 6Gi
                    cpu: "2"
            """
        }
    }
    stages {
        stage('build image') {
            steps {
                container('podman') {
                    sh 'podman build --platform linux/arm64 --tag $IMAGE_NAME -f ./Dockerfile .'
                }
            }
        }
        stage('push image') {
            when { branch 'main' }
            steps {
                container('podman') {
                    sh 'podman push $IMAGE_NAME $REGISTRY_URL/$IMAGE_NAME:$GIT_SHORT_HASH --creds=$REGISTRY_CREDS --tls-verify=false'
                }
            }
        }
        stage('update k8s deployment') {
            when { branch 'main' }
            steps {
                container('yq') {
                    sh 'yq -i ".spec.template.spec.containers[0].image |= sub(\\\":[^:]+$\\\", \\\":$GIT_SHORT_HASH\\\")" $DEPLOYMENT_MANIFEST'
                    sh 'grep "image:" $DEPLOYMENT_MANIFEST'
                }
                withCredentials([usernamePassword(credentialsId: 'github-token-creds', usernameVariable: 'GIT_USERNAME', passwordVariable: 'GIT_PASSWORD')]) {
                    sh 'git config --global user.name "Jenkins"'
                    sh 'git config --global user.email "jenkins@raspberrypile.com"'
                    sh 'git checkout main'
                    sh 'git add $DEPLOYMENT_MANIFEST'
                    sh 'git commit -m "deploy: evalien $GIT_SHORT_HASH"'
                    sh 'git push https://${GIT_USERNAME}:${GIT_PASSWORD}@github.com/${REPO_SLUG}.git main'
                }
            }
        }
    }
}
