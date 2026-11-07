module Api
  class PostsController < ApplicationController
    before_action :set_post, only: [:show, :update, :destroy]

    def index
      render json: Post.recent
    end

    def show
      render json: @post
    end

    def create
      post = Post.new(post_params)
      if post.save
        render json: post, status: :created
      else
        render json: { error: post.errors.full_messages.join(", ") },
               status: :unprocessable_content
      end
    end

    def update
      if @post.update(post_params)
        render json: @post
      else
        render json: { error: @post.errors.full_messages.join(", ") },
               status: :unprocessable_content
      end
    end

    def destroy
      @post.destroy
      head :no_content
    end

    def protected
      raw = request.headers["Authorization"]&.delete_prefix("Bearer ")
      if raw.blank?
        return render json: { error: "missing or invalid Authorization header" },
                      status: :unauthorized
      end
      principal = ApiToken.verify(raw)
      if principal.nil?
        return render json: { error: "invalid token" }, status: :unauthorized
      end
      render json: { principal: principal, total_posts: Post.count }
    end

    private

    def set_post
      @post = Post.find(params[:id])
    end

    def post_params
      source = params[:post].present? ? params.require(:post) : params
      source.permit(:title, :body, :published, :author)
    end
  end
end
